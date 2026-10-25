//! Usage checks (spec section 20): reading an account's usage without a turn, by running Claude
//! Code's own `/usage` with the account's login in a scratch profile.
//!
//! `claude -p /usage` asks `GET /api/oauth/usage` for the account's usage and caches the answer
//! in the scratch profile's `.claude.json`, stamped with the account it fetched for, whose
//! identity Claude Code fetches itself (F17). It makes no model request and uses none of the
//! account's quota; Claude Code's JSON result says so (`num_turns` 0, `local_command` `usage`),
//! and a result saying otherwise stops all checks. cc-rotate itself sends nothing.
//!
//! Accounts that are not in use are checked with their stored login, which Claude Code renews
//! when its access token expired, retiring the refresh token the store holds. So:
//! - the account's lock (`Paths::account_lock`) is held from before its login is read until the
//!   outcome is stored, and a swap and a re-login take it too, so no two of them ever use one
//!   login at once;
//! - a renewed login is stored in the same transaction as the reading;
//! - the scratch profile is deleted only after that, and never while its login cannot be read.
//!   A check that dies first leaves it behind, and the next holder of the lock recovers the
//!   renewal from it (`recover`), judged by the login the check started from (`start.fp`);
//! - Claude Code runs as a process tree that is killed as a whole on a timeout (`process::Tree`);
//! - when it is unclear whether Claude Code renewed the login (a timeout, a login that could not
//!   be read), the account is held back from swaps until a check settles it, and that check
//!   makes Claude Code renew: the login either works (and is stored) or is rejected.
//!
//! The account in use is only read to confirm a threshold swap (`confirm_active`), with a copy
//! of the profile's login whose refresh token is removed, so Claude Code cannot renew it.
//!
//! The usage endpoint limits how often each login may ask: it answers 429 with a `retry-after` of
//! up to an hour. Claude Code logs that wait in its debug log, and automatic checks of the
//! account do not run again before it ends.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::credstore::Credentials;
use crate::lock::{DirLock, LockError, LockOptions};
use crate::paths::Paths;
use crate::process::{self, Tree};
use crate::store::{Account, AccountState, Sample, SampleWindow, Store, kind};
use crate::{credfile, credstore, login, profile_usage, scheduler, swap, time, usage_report};

/// How long Claude Code may take: it starts up, fetches its settings, then the usage.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(60);
/// Before a swap, a check may not hold the swap up for long.
const BEFORE_SWAP_TIMEOUT: Duration = Duration::from_secs(30);
/// The wait after a 429 when Claude Code did not log one.
const DEFAULT_RETRY_AFTER_MS: i64 = 5 * 60_000;
/// Claude Code never holds a 429 longer than this, and neither does cc-rotate.
const MAX_RETRY_AFTER_MS: i64 = 3_600_000;
/// A held-back account is checked again at most this often.
const HELD_BACK_RETRY_MS: i64 = 5 * 60_000;
/// Names of scratch profiles: `check-<account id>-<start ms>-<uuid>`.
const SCRATCH_PREFIX: &str = "check-";
/// Scratch profiles of `confirm_active`: `confirm-<uuid>`. They hold a login that cannot be
/// renewed, so nothing in them is ever recovered.
const CONFIRM_PREFIX: &str = "confirm-";
/// Files a check leaves in its scratch profile.
const PID_FILE: &str = "claude.pid";
const START_FP_FILE: &str = "start.fp";
const OUT_FILE: &str = "out.json";
const ERR_FILE: &str = "err.log";
const DEBUG_FILE: &str = "debug.log";
/// Prefix of the meta keys holding each account's `CheckState`.
const META_STATE_PREFIX: &str = "usage_check:";
/// Set (to the reason) when checks must not run until the user turns them on again: Claude Code
/// ran `/usage` as a prompt, or keeps logins where cc-rotate cannot read them. Setting
/// `usage_checks` clears it (`Store::set_setting`).
pub const META_BLOCKED: &str = "usage_checks_blocked";
/// The sample profile name of a check's readings.
pub const SOURCE: &str = "check";

/// Prefixes of environment variables that would make Claude Code use another login, account or
/// backend than the scratch profile's, or tie it to a session or tool it was started from.
const SCRUBBED_PREFIXES: &[&str] = &["ANTHROPIC_", "CLAUDE_CODE_", "CLAUDECODE", "HERDR_"];

/// Who asked for a check, which decides what may stop it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The user asked: runs whatever the settings and back-offs say.
    Manual,
    /// A swap is about to use the account and its latest reading is old.
    BeforeSwap,
    /// The app keeps the dashboard current.
    Background,
}

/// What cc-rotate remembers about an account's checks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CheckState {
    /// When the last check started (epoch ms).
    pub last_attempt_at: Option<i64>,
    /// When a check last read the account's usage (epoch ms).
    pub last_read_at: Option<i64>,
    /// No automatic check before this (epoch ms): the usage endpoint's wait after it refused, or
    /// a back-off after a failure.
    pub not_before: Option<i64>,
    /// Why the last check failed, for people. A refusal by the usage endpoint is not a
    /// failure; it only sets `not_before`.
    pub problem: Option<String>,
    /// It is unclear whether Claude Code renewed the stored login during a check (it was
    /// interrupted, or what it left could not be read): the account is not used until a check
    /// settles it.
    #[serde(default)]
    pub held_back: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CheckOutcome {
    /// The usage was read and recorded.
    Read {
        windows: Vec<SampleWindow>,
    },
    /// The check did not run.
    Skipped {
        why: String,
    },
    /// The usage endpoint refused for now; automatic checks wait until `until` (epoch ms).
    RateLimited {
        until: i64,
    },
    /// Anthropic rejected the stored login; the account needs a new one.
    LoginRejected,
    Failed {
        why: String,
    },
}

impl CheckOutcome {
    /// A word for events and the CLI.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Read { .. } => "read",
            Self::Skipped { .. } => "skipped",
            Self::RateLimited { .. } => "rate_limited",
            Self::LoginRejected => "login_rejected",
            Self::Failed { .. } => "failed",
        }
    }
}

/// What a run of Claude Code left behind.
#[derive(Debug, Default)]
struct Run {
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: String,
    stderr: String,
}

pub fn state(store: &Store, account_id: i64) -> Result<CheckState> {
    Ok(store
        .meta(&state_key(account_id))?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default())
}

fn state_key(account_id: i64) -> String {
    format!("{META_STATE_PREFIX}{account_id}")
}

fn save_state(store: &Store, account_id: i64, state: &CheckState) -> Result<()> {
    store.set_meta(&state_key(account_id), serde_json::to_string(state)?)
}

/// Forget an account's checks: after a fresh login, and when the account is removed (a new
/// account may get its id).
pub fn forget(store: &Store, account_id: i64) -> Result<()> {
    store.delete_meta(&state_key(account_id))
}

/// Why checks are stopped until the user turns them on again, if they are.
pub fn blocked(store: &Store) -> Result<Option<String>> {
    store.meta(META_BLOCKED)
}

/// Whether nothing at all keeps automatic checks from running.
fn checks_possible(store: &Store, paths: &Paths) -> Result<Option<String>> {
    if let Some(reason) = blocked(store)? {
        return Ok(Some(format!("usage checks are stopped: {reason}")));
    }
    if cfg!(windows)
        && credstore::windows_credman_enabled(
            &paths.profile_dir,
            std::env::var("CLAUDE_CODE_FORCE_WINDOWS_CREDMAN")
                .ok()
                .as_deref(),
        )
    {
        return Ok(Some(
            "Claude Code keeps logins in the Windows Credential Manager on this computer, which usage checks do not support".into(),
        ));
    }
    Ok(None)
}

/// Whether a swap about to use `account` should check it first: checks are on, its latest
/// reading is older than `usage_check_max_age_s` (or there is none), and no back-off is running.
pub fn due_before_swap(store: &Store, account: &Account, now_ms: i64) -> Result<bool> {
    let settings = store.settings()?;
    if !settings.usage_checks || account.state != AccountState::Parked || blocked(store)?.is_some()
    {
        return Ok(false);
    }
    let st = state(store, account.id)?;
    if st.held_back || st.not_before.is_some_and(|t| t > now_ms) {
        return Ok(false);
    }
    let fresh_since = now_ms - settings.usage_check_max_age_s * 1000;
    Ok(store
        .last_sample(account.id)?
        .is_none_or(|s| s.ts < fresh_since))
}

/// The account the background should check next, if any, in the order the scheduler would use
/// them: first a held-back account (not tried in the last few minutes), then a parked account
/// whose latest reading and last check are both older than `usage_check_interval_s` and whose
/// back-off is over.
pub fn next_due(store: &Store, paths: &Paths, now_ms: i64) -> Result<Option<i64>> {
    let settings = store.settings()?;
    if !settings.usage_checks || checks_possible(store, paths)?.is_some() {
        return Ok(None);
    }
    let older_than = now_ms - settings.usage_check_interval_s * 1000;
    let (pool, policy) = swap::load_pool(store, now_ms / 1000)?;
    let ranked = scheduler::ranking(&pool, policy, now_ms / 1000);
    let parked = || {
        ranked
            .iter()
            .filter_map(|r| pool.iter().find(|c| c.account.id == r.account_id))
            .filter(|c| c.account.state == AccountState::Parked)
    };
    for c in parked() {
        let st = state(store, c.account.id)?;
        if st.held_back
            && st
                .last_attempt_at
                .is_none_or(|t| now_ms - t > HELD_BACK_RETRY_MS)
        {
            return Ok(Some(c.account.id));
        }
    }
    for c in parked() {
        let st = state(store, c.account.id)?;
        let waiting = st.held_back || st.not_before.is_some_and(|t| t > now_ms);
        let tried = st.last_attempt_at.is_some_and(|t| t > older_than);
        let read = store
            .last_sample(c.account.id)?
            .is_some_and(|s| s.ts > older_than);
        if !waiting && !tried && !read {
            return Ok(Some(c.account.id));
        }
    }
    Ok(None)
}

/// Check one account's usage with Claude Code's `/usage`.
pub fn check(
    store: &Store,
    paths: &Paths,
    account_id: i64,
    trigger: Trigger,
) -> Result<CheckOutcome> {
    let timeout = match trigger {
        Trigger::BeforeSwap => BEFORE_SWAP_TIMEOUT,
        _ => RUN_TIMEOUT,
    };
    check_with(store, paths, account_id, trigger, &|scratch| {
        run_claude(&login::claude_executable(store)?, scratch, timeout)
    })
}

fn check_with(
    store: &Store,
    paths: &Paths,
    account_id: i64,
    trigger: Trigger,
    run: &dyn Fn(&Path) -> Result<Run>,
) -> Result<CheckOutcome> {
    let skipped = |why: &str| {
        Ok(CheckOutcome::Skipped {
            why: why.to_string(),
        })
    };
    let settings = store.settings()?;
    if !settings.usage_checks && trigger != Trigger::Manual {
        return skipped("usage checks are off");
    }
    if let Some(why) = checks_possible(store, paths)? {
        return skipped(&why);
    }
    let mut st = state(store, account_id)?;
    let now = time::now_ms();
    if trigger != Trigger::Manual && !st.held_back && st.not_before.is_some_and(|t| t > now) {
        return skipped("waiting until the usage endpoint answers again");
    }
    // A skip below is about this account: count it as an attempt, so the background moves on
    // to the next account instead of picking this one every minute.
    let attempted = |store: &Store, why: &str| -> Result<CheckOutcome> {
        let mut st = state(store, account_id)?;
        st.last_attempt_at = Some(time::now_ms());
        save_state(store, account_id, &st)?;
        skipped(why)
    };

    let wait = match trigger {
        Trigger::Manual => Duration::from_millis(settings.lock_timeout_ms),
        _ => Duration::ZERO,
    };
    let _lock = match DirLock::acquire(&paths.account_lock(account_id), LockOptions::claude(wait)) {
        Ok(lock) => lock,
        Err(LockError::Busy(_)) => {
            return attempted(
                store,
                "another check or a swap is using this account's login",
            );
        }
        Err(e) => return Err(e.into()),
    };
    if let Recovery::Busy(why) = recover(store, paths, account_id)? {
        return attempted(store, &why);
    }

    // Under the lock, so a swap cannot make it active while Claude Code runs with its login.
    let Some(account) = store.account(account_id)? else {
        return skipped("the account no longer exists");
    };
    match account.state {
        AccountState::Active => return skipped("it is in use; its status line reports its usage"),
        AccountState::NeedsLogin => return skipped("it needs a new login"),
        AccountState::Paused if trigger != Trigger::Manual => return skipped("it is paused"),
        _ => {}
    }
    st = state(store, account_id)?;
    let stored = match store.credential(account_id) {
        Ok(Some(c)) => c,
        Ok(None) => return skipped("it has no stored login"),
        Err(e) => {
            let why = format!("its stored login cannot be read: {e:#}");
            st.last_attempt_at = Some(time::now_ms());
            st.problem = Some(why.clone());
            save_state(store, account_id, &st)?;
            return Ok(CheckOutcome::Failed { why });
        }
    };

    let started = time::now_ms();
    st.last_attempt_at = Some(started);
    save_state(store, account_id, &st)?;

    let mut scratch = Scratch::create(paths, SCRATCH_PREFIX, Some((account_id, started)))?;
    let mut oauth = stored.oauth.clone();
    if st.held_back {
        // Settle whether the stored login still works: make Claude Code renew it.
        oauth["expiresAt"] = json!(0);
    }
    credfile::write_oauth(&scratch.dir.join(".credentials.json"), &oauth)?;
    std::fs::write(scratch.dir.join(START_FP_FILE), &stored.refresh_fp)?;

    // From here on Claude Code may renew the login: keep the scratch profile until the outcome
    // is stored.
    scratch.keep = true;
    let ran = run(&scratch.dir);

    let left = match Credentials::for_profile(&scratch.dir).read_oauth() {
        Ok(left) => left,
        Err(e) => {
            // Claude Code may have renewed the login and the only copy is in there: keep it for
            // `recover`, and keep the account out of swaps until then.
            let why = format!(
                "the login Claude Code left after the check cannot be read ({e:#}); the account is not used until a check settles it"
            );
            st.held_back = true;
            st.problem = Some(why.clone());
            save_state(store, account_id, &st)?;
            return Ok(CheckOutcome::Failed { why });
        }
    };
    let config = credfile::read_document(&scratch.dir.join(".claude.json"))
        .ok()
        .flatten();
    let report = config
        .as_ref()
        .and_then(usage_report::from_config)
        .filter(|r| r.fetched_at_ms >= started - 1000);
    let debug_log = std::fs::read_to_string(scratch.dir.join(DEBUG_FILE)).unwrap_or_default();
    let credman = cfg!(windows) && credstore::windows_credman_enabled(&scratch.dir, None);
    let timed_out = ran.as_ref().is_ok_and(|r| r.timed_out);
    let prompted = ran.as_ref().ok().and_then(|r| ran_as_prompt(&r.stdout));

    let dead = left.as_ref().is_some_and(credfile::is_dead);
    let renewal = left
        .filter(|o| !credfile::is_dead(o))
        .filter(|o| credfile::oauth_fingerprint(o).as_deref() != Some(stored.refresh_fp.as_str()));
    let mut wrong_account = None;

    let outcome = if let Some(why) = &prompted {
        CheckOutcome::Failed { why: why.clone() }
    } else if dead {
        CheckOutcome::LoginRejected
    } else if let Some(report) = report {
        match (&account.account_uuid, &report.account_uuid) {
            (Some(expected), Some(got)) if expected != got => {
                wrong_account = Some(got.clone());
                CheckOutcome::Failed {
                    why: format!(
                        "the stored login belongs to another account ({got}); sign this account in again"
                    ),
                }
            }
            (Some(_), None) => CheckOutcome::Failed {
                why: "Claude Code did not say which account the usage belongs to".into(),
            },
            _ => CheckOutcome::Read {
                windows: report.windows,
            },
        }
    } else if let Some(wait) = rate_limited(&debug_log) {
        CheckOutcome::RateLimited {
            until: time::now_ms() + wait,
        }
    } else {
        CheckOutcome::Failed {
            why: failure(&ran, &debug_log),
        }
    };

    // The login was exercised one way or the other; anything else leaves the question open.
    let settled = renewal.is_some() || dead || wrong_account.is_some();
    if st.held_back && settled {
        st.held_back = false;
    }
    if (timed_out || credman) && !settled {
        st.held_back = true;
    }

    let now = time::now_ms();
    store.immediate(|s| {
        if let Some(oauth) = &renewal {
            s.save_credential(account_id, oauth, SOURCE)?;
            s.log_event(kind::SYNC, Some(account_id), None, Some("usage_check"), None)?;
        }
        if dead {
            s.set_state(account_id, AccountState::NeedsLogin)?;
            s.log_event(
                kind::DEAD_LOGIN,
                Some(account_id),
                None,
                Some("usage_check"),
                None,
            )?;
        }
        if let Some(got) = &wrong_account {
            s.set_state(account_id, AccountState::NeedsLogin)?;
            s.log_event(
                kind::ERROR,
                Some(account_id),
                None,
                Some("usage_check"),
                Some(json!({ "message": format!("{}'s stored login belongs to another account ({got}); it needs a new login", account.label) })),
            )?;
        }
        if let Some(why) = &prompted {
            block(s, why)?;
        }
        if credman {
            block(
                s,
                "Claude Code started keeping logins in the Windows Credential Manager, which usage checks do not support",
            )?;
        }
        match &outcome {
            CheckOutcome::Read { windows } => {
                let last = s.last_sample(account_id)?;
                s.record_sample(&Sample {
                    ts: now,
                    account_id,
                    profile: SOURCE.into(),
                    session_id: None,
                    model: None,
                    windows: profile_usage::with_carried(windows.clone(), last.as_ref(), now / 1000),
                })?;
                st.last_read_at = Some(now);
                st.not_before = None;
                st.problem = None;
            }
            // Routine: the endpoint allows a reading now and then, not a problem to report.
            CheckOutcome::RateLimited { until } => st.not_before = Some(*until),
            CheckOutcome::Failed { why } => {
                st.not_before = Some(now + s.settings()?.usage_check_interval_s * 1000);
                st.problem = Some(why.clone());
            }
            CheckOutcome::LoginRejected => {
                st.problem = Some("Anthropic rejected the stored login".into());
            }
            CheckOutcome::Skipped { .. } => {}
        }
        if st.held_back && st.problem.is_none() {
            st.problem = Some(if timed_out {
                "a check was interrupted while Claude Code may have been renewing the login; the account is not used until a check settles it".into()
            } else {
                "the account is not used until a check settles whether its login still works".into()
            });
        }
        save_state(s, account_id, &st)
    })?;
    scratch.keep = false;
    Ok(outcome)
}

/// Stop checks until the user turns them on again, and say why.
fn block(store: &Store, reason: &str) -> Result<()> {
    if blocked(store)?.is_some() {
        return Ok(());
    }
    store.set_meta(META_BLOCKED, reason)?;
    store.log_event(
        kind::ERROR,
        None,
        None,
        Some("usage_check"),
        Some(json!({ "message": format!("Usage checks are stopped: {reason}. Turn them on again with `cc-rotate-ctl config usage_checks true` once Claude Code is updated.") })),
    )?;
    Ok(())
}

/// Why a result is not that of the `/usage` command: Claude Code ran the text as a prompt (a
/// model request, using the account's quota), which no check may ever do again.
fn ran_as_prompt(stdout: &str) -> Option<String> {
    let result: Value = serde_json::from_str(stdout.trim()).ok()?;
    let turns = result.get("num_turns").and_then(Value::as_i64).unwrap_or(0);
    let command = result.get("local_command").and_then(Value::as_str);
    (turns > 0 || command != Some("usage")).then(|| {
        "Claude Code ran /usage as a prompt instead of its own command, which uses the account's limits".to_string()
    })
}

/// What `recover` found.
#[derive(Debug, PartialEq, Eq)]
pub enum Recovery {
    Clean,
    /// The account's login must not be used now: a check of it is still running, or what one
    /// left cannot be read yet.
    Busy(String),
}

/// Settle what earlier checks of this account that died left in their scratch profiles: a
/// renewed login is stored and a rejected one marks the account, as long as the stored login is
/// still the one the check started from (`start.fp`); otherwise (a re-login since) the leftover
/// is stale and discarded. Call with the account's lock held.
pub fn recover(store: &Store, paths: &Paths, account_id: i64) -> Result<Recovery> {
    let prefix = format!("{SCRATCH_PREFIX}{account_id}-");
    for dir in scratch_dirs(paths, &prefix) {
        if running(&dir) {
            return Ok(Recovery::Busy(
                "a usage check of this account is still running".into(),
            ));
        }
        let left = match Credentials::for_profile(&dir).read_oauth() {
            Ok(left) => left,
            Err(_) => {
                hold_back(store, account_id)?;
                return Ok(Recovery::Busy(
                    "the login an earlier usage check left cannot be read yet".into(),
                ));
            }
        };
        let start = std::fs::read_to_string(dir.join(START_FP_FILE))
            .ok()
            .map(|s| s.trim().to_string());
        let stored = store.credential_meta(account_id)?;
        match (left, start, stored) {
            (Some(oauth), Some(start), Some(stored)) if stored.refresh_fp == start => {
                if credfile::is_dead(&oauth) {
                    store.immediate(|s| {
                        s.set_state(account_id, AccountState::NeedsLogin)?;
                        s.log_event(
                            kind::DEAD_LOGIN,
                            Some(account_id),
                            None,
                            Some("usage_check"),
                            None,
                        )
                        .map(drop)
                    })?;
                } else if credfile::oauth_fingerprint(&oauth).as_deref() != Some(start.as_str()) {
                    store.immediate(|s| {
                        s.save_credential(account_id, &oauth, SOURCE)?;
                        s.log_event(kind::SYNC, Some(account_id), None, Some("recovered"), None)?;
                        let mut st = state(s, account_id)?;
                        st.held_back = false;
                        save_state(s, account_id, &st)
                    })?;
                }
            }
            // A re-login stored a newer login since the check began: what it left is stale.
            (_, Some(_), Some(_)) => {}
            // Nothing to judge by (no login left, or no record of the start): the stored login
            // may be retired, so settle it with the next check.
            (_, _, Some(_)) => hold_back(store, account_id)?,
            (_, _, None) => {}
        }
        remove_scratch(&dir);
    }
    Ok(Recovery::Clean)
}

fn hold_back(store: &Store, account_id: i64) -> Result<()> {
    let mut st = state(store, account_id)?;
    if !st.held_back {
        st.held_back = true;
        save_state(store, account_id, &st)?;
    }
    Ok(())
}

/// Settle every scratch profile left behind (at app start): the ones of accounts whose lock is
/// free are recovered, those of removed accounts and of `confirm_active` are deleted.
pub fn sweep(store: &Store, paths: &Paths) -> Result<()> {
    for dir in scratch_dirs(paths, CONFIRM_PREFIX) {
        if !running(&dir) {
            remove_scratch(&dir);
        }
    }
    let mut ids: Vec<i64> = scratch_dirs(paths, SCRATCH_PREFIX)
        .iter()
        .filter_map(|d| {
            d.file_name()?
                .to_str()?
                .strip_prefix(SCRATCH_PREFIX)?
                .split('-')
                .next()?
                .parse()
                .ok()
        })
        .collect();
    ids.sort_unstable();
    ids.dedup();
    for id in ids {
        if store.account(id)?.is_none() {
            for dir in scratch_dirs(paths, &format!("{SCRATCH_PREFIX}{id}-")) {
                if !running(&dir) {
                    remove_scratch(&dir);
                }
            }
            continue;
        }
        if let Ok(_lock) =
            DirLock::acquire(&paths.account_lock(id), LockOptions::claude(Duration::ZERO))
        {
            recover(store, paths, id)?;
        }
    }
    Ok(())
}

/// Remove an account, its check state and anything its checks left behind, under its lock so no
/// check is using its login meanwhile.
pub fn remove_account(store: &Store, paths: &Paths, account_id: i64) -> Result<()> {
    let wait = Duration::from_millis(store.settings()?.lock_timeout_ms) + RUN_TIMEOUT;
    let _lock = DirLock::acquire(&paths.account_lock(account_id), LockOptions::claude(wait))
        .context("waiting for a usage check of the account to finish")?;
    let dirs = scratch_dirs(paths, &format!("{SCRATCH_PREFIX}{account_id}-"));
    if dirs.iter().any(|d| running(d)) {
        bail!("a usage check of this account is still running; try again in a minute");
    }
    for dir in dirs {
        remove_scratch(&dir);
    }
    forget(store, account_id)?;
    store.remove_account(account_id)
}

/// Read the account in use's usage, to confirm a threshold swap that rests on a reading the
/// status line cannot attribute for sure (spec section 20.4). Uses a copy of the profile's
/// login without its refresh token, so Claude Code cannot renew (and so retire) the login the
/// profile's sessions use; when the access token is about to expire there is no check.
pub fn confirm_active(store: &Store, paths: &Paths) -> Result<CheckOutcome> {
    confirm_with(store, paths, &|scratch| {
        run_claude(
            &login::claude_executable(store)?,
            scratch,
            BEFORE_SWAP_TIMEOUT,
        )
    })
}

fn confirm_with(
    store: &Store,
    paths: &Paths,
    run: &dyn Fn(&Path) -> Result<Run>,
) -> Result<CheckOutcome> {
    let skipped = |why: &str| {
        Ok(CheckOutcome::Skipped {
            why: why.to_string(),
        })
    };
    if !store.settings()?.usage_checks {
        return skipped("usage checks are off");
    }
    if let Some(why) = checks_possible(store, paths)? {
        return skipped(&why);
    }
    let Some(active) = store.active_account()? else {
        return skipped("no account is in use");
    };
    let Some(mut oauth) = paths.credentials().read_oauth()? else {
        return skipped("the profile has no login");
    };
    let expires = oauth.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    if expires < time::now_ms() + 120_000 {
        return skipped("its access token is about to be renewed");
    }
    oauth["refreshToken"] = json!("");
    let started = time::now_ms();
    let scratch = Scratch::create(paths, CONFIRM_PREFIX, None)?;
    credfile::write_oauth(&scratch.dir.join(".credentials.json"), &oauth)?;
    let ran = run(&scratch.dir);
    let report = credfile::read_document(&scratch.dir.join(".claude.json"))
        .ok()
        .flatten()
        .as_ref()
        .and_then(usage_report::from_config)
        .filter(|r| r.fetched_at_ms >= started - 1000);
    let debug_log = std::fs::read_to_string(scratch.dir.join(DEBUG_FILE)).unwrap_or_default();
    if let Some(why) = ran.as_ref().ok().and_then(|r| ran_as_prompt(&r.stdout)) {
        block(store, &why)?;
        return Ok(CheckOutcome::Failed { why });
    }
    let Some(report) = report else {
        return Ok(match rate_limited(&debug_log) {
            Some(wait) => CheckOutcome::RateLimited {
                until: time::now_ms() + wait,
            },
            None => CheckOutcome::Failed {
                why: failure(&ran, &debug_log),
            },
        });
    };
    if active.account_uuid.is_none() || report.account_uuid != active.account_uuid {
        return Ok(CheckOutcome::Failed {
            why: "the profile's login did not report the usage of the account in use".into(),
        });
    }
    let now = time::now_ms();
    let last = store.last_sample(active.id)?;
    let windows = profile_usage::with_carried(report.windows, last.as_ref(), now / 1000);
    store.record_sample(&Sample {
        ts: now,
        account_id: active.id,
        profile: SOURCE.into(),
        session_id: None,
        model: None,
        windows: windows.clone(),
    })?;
    // The account's own reading: it vouches for its reset times.
    swap::note_known_resets(store, active.id, &windows, now / 1000)?;
    Ok(CheckOutcome::Read { windows })
}

/// Whether the Claude Code a scratch profile records as started is still running.
fn running(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(PID_FILE))
        .ok()
        .and_then(|p| p.trim().parse::<u32>().ok())
        .is_some_and(process::alive)
}

fn scratch_dirs(paths: &Paths, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(paths.staging_root()) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

/// A scratch profile for one check. Removed when dropped, unless `keep` is set (Claude Code may
/// have renewed the login in it and the renewal is not stored yet).
struct Scratch {
    dir: PathBuf,
    keep: bool,
}

impl Scratch {
    /// `check-<account id>-<start ms>-<uuid>` for an account's check, `confirm-<uuid>` else.
    fn create(paths: &Paths, prefix: &str, account: Option<(i64, i64)>) -> Result<Scratch> {
        let unique = uuid::Uuid::new_v4().simple().to_string();
        let name = match account {
            Some((id, started)) => format!("{prefix}{id}-{started}-{unique}"),
            None => format!("{prefix}{unique}"),
        };
        let dir = paths.staging_root().join(name);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Scratch { dir, keep: false })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.keep {
            remove_scratch(&self.dir);
        }
    }
}

/// On macOS a renewed login lands in a Keychain item named after the directory; it goes too.
fn remove_scratch(dir: &Path) {
    let _ = Credentials::for_profile(dir).delete_keychain_item();
    let _ = std::fs::remove_dir_all(dir);
}

/// `claude -p /usage` in the scratch profile. Its output goes to files there (no pipes a stray
/// process could hold open), its process tree is killed after `timeout`, and its process id is
/// recorded so `recover` never touches a scratch profile whose Claude Code is still running.
fn run_claude(claude: &Path, scratch: &Path, timeout: Duration) -> Result<Run> {
    let out = std::fs::File::create(scratch.join(OUT_FILE))?;
    let err = std::fs::File::create(scratch.join(ERR_FILE))?;
    let mut cmd = Command::new(claude);
    cmd.args(["-p", "/usage", "--output-format", "json", "--debug-file"])
        .arg(scratch.join(DEBUG_FILE))
        .current_dir(scratch)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if SCRUBBED_PREFIXES.iter().any(|p| name.starts_with(p)) {
            cmd.env_remove(&key);
        }
    }
    cmd.env("CLAUDE_CONFIG_DIR", scratch)
        .env("DISABLE_AUTOUPDATER", "1");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Callers include the desktop app and the background swap, which have no console.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut tree =
        Tree::spawn(&mut cmd).with_context(|| format!("running {}", claude.display()))?;
    std::fs::write(scratch.join(PID_FILE), tree.id().to_string())?;
    let deadline = Instant::now() + timeout;
    let mut run = Run::default();
    loop {
        if let Some(status) = tree.try_wait()? {
            run.exit_code = status.code();
            break;
        }
        if Instant::now() >= deadline {
            tree.kill();
            run.timed_out = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Whatever it started and left running goes with it.
    drop(tree);
    let _ = std::fs::remove_file(scratch.join(PID_FILE));
    run.stdout = std::fs::read_to_string(scratch.join(OUT_FILE)).unwrap_or_default();
    run.stderr = std::fs::read_to_string(scratch.join(ERR_FILE)).unwrap_or_default();
    Ok(run)
}

/// How long the usage endpoint asked to wait, when Claude Code's debug log says it refused.
fn rate_limited(debug_log: &str) -> Option<i64> {
    let refused = debug_log.lines().any(|l| {
        (l.contains("fetchUtilization") || l.contains("Failed to load usage data"))
            && l.contains("429")
    });
    if !refused {
        return None;
    }
    let wait = regex::Regex::new(r"not asking again for (\d+)s")
        .ok()
        .and_then(|re| re.captures(debug_log))
        .and_then(|c| c[1].parse::<i64>().ok())
        .map_or(DEFAULT_RETRY_AFTER_MS, |s| s * 1000);
    Some(wait.clamp(60_000, MAX_RETRY_AFTER_MS))
}

/// A short reason a run did not produce a reading.
fn failure(ran: &Result<Run>, debug_log: &str) -> String {
    let run = match ran {
        Ok(run) => run,
        Err(e) => return format!("{e:#}"),
    };
    if run.timed_out {
        return "Claude Code did not finish in time".into();
    }
    let logged = debug_log
        .lines()
        .rev()
        .find(|l| l.contains("Failed to load usage data"))
        .and_then(|l| l.split("Failed to load usage data: ").nth(1));
    if let Some(why) = logged {
        return format!("Claude Code could not read the usage: {}", why.trim());
    }
    let last = run.stderr.lines().rev().find(|l| !l.trim().is_empty());
    match (run.exit_code, last) {
        (Some(0), _) => "Claude Code did not report any usage".into(),
        (code, Some(line)) => format!(
            "Claude Code exited with {}: {}",
            code.map_or("a signal".into(), |c| c.to_string()),
            line.trim()
        ),
        (code, None) => format!(
            "Claude Code exited with {}",
            code.map_or("a signal".into(), |c| c.to_string())
        ),
    }
}

/// The outcome in a few words for people: "5-hour 100%", "Anthropic asked to wait 24m ...".
pub fn describe(outcome: &CheckOutcome) -> String {
    match outcome {
        CheckOutcome::Read { windows } if windows.is_empty() => "no limits reported".into(),
        CheckOutcome::Read { windows } => windows
            .iter()
            .map(|w| format!("{} {:.0}%", scheduler::window_label(&w.name), w.pct))
            .collect::<Vec<_>>()
            .join(", "),
        CheckOutcome::Skipped { why } => format!("skipped: {why}"),
        CheckOutcome::RateLimited { until } => format!(
            "Anthropic asked to wait {} before the next reading",
            time::span((until - time::now_ms()) / 1000)
        ),
        CheckOutcome::LoginRejected => "its login was rejected; sign it in again".into(),
        CheckOutcome::Failed { why } => format!("failed: {why}"),
    }
}

/// Used by `swap` to record what it checked before picking.
pub fn summary(store: &Store, account_id: i64, outcome: &CheckOutcome) -> Value {
    let label = store
        .account(account_id)
        .ok()
        .flatten()
        .map(|a| a.label)
        .unwrap_or_default();
    json!({ "label": label, "outcome": outcome.kind() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::AccountIdentity;

    struct Env {
        _root: tempfile::TempDir,
        paths: Paths,
        store: Store,
    }

    fn env() -> Env {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::new(root.path().join("data"), root.path().join("profile"))
            .with_home(root.path().join("home"));
        std::fs::create_dir_all(&paths.profile_dir).unwrap();
        let store = Store::open(&paths.store_db()).unwrap();
        Env {
            _root: root,
            paths,
            store,
        }
    }

    fn oauth(tag: &str) -> Value {
        json!({
            "accessToken": format!("access-{tag}"),
            "refreshToken": format!("refresh-{tag}"),
            "expiresAt": time::now_ms() + 3_600_000,
            "subscriptionType": "team",
        })
    }

    fn account(e: &Env, label: &str, uuid: &str, state: AccountState) -> i64 {
        let ident = AccountIdentity {
            account_uuid: Some(uuid.into()),
            email: Some(format!("{label}@x.test")),
            ..Default::default()
        };
        let id = e.store.insert_account(label, &ident, state).unwrap();
        e.store.save_credential(id, &oauth(label), "login").unwrap();
        id
    }

    const USAGE_RESULT: &str = r#"{"type":"result","num_turns":0,"local_command":"usage"}"#;

    /// What a fake Claude Code does in the scratch profile.
    #[derive(Clone)]
    struct Fake {
        /// The account the cached reading is stamped with; `None` writes no reading.
        stamp: Option<String>,
        pct: f64,
        /// Replace the login, as a renewal does; `Some(None)` writes an unreadable file.
        renew: Option<Option<Value>>,
        debug_log: String,
        stdout: String,
        timed_out: bool,
    }

    impl Default for Fake {
        fn default() -> Self {
            Fake {
                stamp: None,
                pct: 0.0,
                renew: None,
                debug_log: String::new(),
                stdout: USAGE_RESULT.into(),
                timed_out: false,
            }
        }
    }

    fn fake(f: Fake) -> impl Fn(&Path) -> Result<Run> {
        move |dir: &Path| {
            let mut config: Value = std::fs::read_to_string(dir.join(".claude.json"))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_else(|| json!({}));
            if let Some(stamp) = &f.stamp {
                config["oauthAccount"] = json!({ "accountUuid": stamp });
                config["cachedUsageUtilization"] = json!({
                    "fetchedAtMs": time::now_ms(),
                    "accountUuid": stamp,
                    "utilization": {"limits": [
                        {"kind": "session", "percent": f.pct, "resets_at": "2099-01-01T00:00:00Z"}
                    ]}
                });
            }
            std::fs::write(dir.join(".claude.json"), config.to_string()).unwrap();
            match &f.renew {
                Some(Some(renew)) => {
                    credfile::write_oauth(&dir.join(".credentials.json"), renew).unwrap()
                }
                Some(None) => std::fs::write(dir.join(".credentials.json"), "{\"claudeAi").unwrap(),
                None => {}
            }
            std::fs::write(dir.join(DEBUG_FILE), &f.debug_log).unwrap();
            Ok(Run {
                exit_code: Some(0),
                timed_out: f.timed_out,
                stdout: f.stdout.clone(),
                ..Default::default()
            })
        }
    }

    fn scratch_dirs_left(e: &Env) -> usize {
        std::fs::read_dir(e.paths.staging_root())
            .map(|d| d.count())
            .unwrap_or(0)
    }

    #[test]
    fn reads_the_usage_and_keeps_a_renewed_login() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let run = fake(Fake {
            stamp: Some("ua".into()),
            pct: 100.0,
            renew: Some(Some(oauth("a-renewed"))),
            ..Default::default()
        });
        let out = check_with(&e.store, &e.paths, a, Trigger::Background, &run).unwrap();
        let CheckOutcome::Read { windows } = &out else {
            panic!("{out:?}")
        };
        assert_eq!(windows[0].name, "five_hour");
        let sample = e.store.last_sample(a).unwrap().unwrap();
        assert_eq!(
            (sample.profile.as_str(), sample.windows[0].pct),
            (SOURCE, 100.0)
        );
        let stored = e.store.credential(a).unwrap().unwrap();
        assert_eq!(stored.oauth["refreshToken"], "refresh-a-renewed");
        assert_eq!(stored.source, SOURCE);
        let st = state(&e.store, a).unwrap();
        assert!(st.last_read_at.is_some() && st.problem.is_none() && !st.held_back);
        assert_eq!(scratch_dirs_left(&e), 0, "the scratch profile is gone");
        assert!(!e.paths.account_lock(a).exists(), "the lock is released");
    }

    #[test]
    fn passes_only_the_stored_login_to_claude_code() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let seen = std::sync::Mutex::new(None);
        let run = |dir: &Path| {
            let creds: Value = serde_json::from_str(
                &std::fs::read_to_string(dir.join(".credentials.json")).unwrap(),
            )
            .unwrap();
            *seen.lock().unwrap() = Some((creds, dir.join(".claude.json").exists()));
            Ok(Run::default())
        };
        check_with(&e.store, &e.paths, a, Trigger::Manual, &run).unwrap();
        let (creds, config) = seen.into_inner().unwrap().unwrap();
        assert_eq!(creds["claudeAiOauth"]["refreshToken"], "refresh-a");
        assert!(
            !config,
            "no identity is given, so Claude Code stamps the reading with the real one"
        );
    }

    #[test]
    fn a_login_that_belongs_to_another_account_marks_the_account() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let run = fake(Fake {
            stamp: Some("someone-else".into()),
            ..Default::default()
        });
        let out = check_with(&e.store, &e.paths, a, Trigger::Manual, &run).unwrap();
        assert!(matches!(out, CheckOutcome::Failed { .. }), "{out:?}");
        assert!(e.store.last_sample(a).unwrap().is_none());
        assert_eq!(
            e.store.account(a).unwrap().unwrap().state,
            AccountState::NeedsLogin
        );
    }

    #[test]
    fn a_prompt_instead_of_the_command_stops_every_check() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let run = fake(Fake {
            stamp: Some("ua".into()),
            stdout: r#"{"type":"result","num_turns":1,"result":"Here is..."}"#.into(),
            ..Default::default()
        });
        let out = check_with(&e.store, &e.paths, a, Trigger::Manual, &run).unwrap();
        assert!(matches!(out, CheckOutcome::Failed { .. }), "{out:?}");
        assert!(blocked(&e.store).unwrap().is_some());
        let never = |_: &Path| -> Result<Run> { panic!("no check may run") };
        for trigger in [Trigger::Manual, Trigger::Background, Trigger::BeforeSwap] {
            assert!(matches!(
                check_with(&e.store, &e.paths, a, trigger, &never).unwrap(),
                CheckOutcome::Skipped { .. }
            ));
        }
        assert_eq!(next_due(&e.store, &e.paths, time::now_ms()).unwrap(), None);
        // Turning checks on again lifts it.
        e.store.set_setting("usage_checks", Some("true")).unwrap();
        assert!(blocked(&e.store).unwrap().is_none());
    }

    #[test]
    fn a_refusal_waits_as_long_as_the_endpoint_asked() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let run = fake(Fake {
            debug_log: "[DEBUG] fetchUtilization: 429 remembered for this bearer; not asking again for 1454s\n[ERROR] Failed to load usage data: Request failed with status code 429\n".into(),
            ..Default::default()
        });
        let before = time::now_ms();
        let out = check_with(&e.store, &e.paths, a, Trigger::Background, &run).unwrap();
        let CheckOutcome::RateLimited { until } = out else {
            panic!("{out:?}")
        };
        assert!((until - before - 1_454_000).abs() < 5_000);
        assert_eq!(state(&e.store, a).unwrap().not_before, Some(until));
        // Automatic checks wait; the user can still ask.
        assert!(matches!(
            check_with(&e.store, &e.paths, a, Trigger::Background, &run).unwrap(),
            CheckOutcome::Skipped { .. }
        ));
        let account = e.store.account(a).unwrap().unwrap();
        assert!(!due_before_swap(&e.store, &account, time::now_ms()).unwrap());
        assert!(matches!(
            check_with(&e.store, &e.paths, a, Trigger::Manual, &run).unwrap(),
            CheckOutcome::RateLimited { .. }
        ));
    }

    #[test]
    fn a_rejected_login_marks_the_account() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let mut dead = oauth("a");
        dead["refreshToken"] = json!("");
        dead["accessToken"] = json!("");
        let run = fake(Fake {
            renew: Some(Some(dead)),
            ..Default::default()
        });
        assert_eq!(
            check_with(&e.store, &e.paths, a, Trigger::Background, &run).unwrap(),
            CheckOutcome::LoginRejected
        );
        assert_eq!(
            e.store.account(a).unwrap().unwrap().state,
            AccountState::NeedsLogin
        );
        assert_eq!(
            e.store
                .last_event_of(kind::DEAD_LOGIN)
                .unwrap()
                .unwrap()
                .account_from,
            Some(a)
        );
    }

    /// Claude Code may have renewed the login when what it left cannot be read: the scratch
    /// profile stays, the account is held back from swaps, and the next check makes Claude Code
    /// renew to settle it.
    #[test]
    fn a_login_left_unreadable_is_kept_and_the_account_held_back() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let run = fake(Fake {
            stamp: Some("ua".into()),
            renew: Some(None),
            ..Default::default()
        });
        let out = check_with(&e.store, &e.paths, a, Trigger::Background, &run).unwrap();
        assert!(matches!(out, CheckOutcome::Failed { .. }), "{out:?}");
        assert_eq!(scratch_dirs_left(&e), 1, "the scratch profile is kept");
        assert!(state(&e.store, a).unwrap().held_back);
        let (pool, policy) = swap::load_pool(&e.store, time::now_s()).unwrap();
        assert!(
            scheduler::pick(&pool, policy, None).is_none(),
            "a held-back account is not picked"
        );
        assert_eq!(
            next_due(&e.store, &e.paths, time::now_ms() + HELD_BACK_RETRY_MS + 1).unwrap(),
            Some(a)
        );
        // Still unreadable: recovery keeps it and the account stays out.
        assert_eq!(
            recover(&e.store, &e.paths, a).unwrap(),
            Recovery::Busy("the login an earlier usage check left cannot be read yet".into())
        );

        // Readable again (a renewal), recovered, and the account usable once more.
        let dir = scratch_dirs(&e.paths, SCRATCH_PREFIX).pop().unwrap();
        std::fs::write(
            dir.join(".credentials.json"),
            json!({ "claudeAiOauth": oauth("a-renewed") }).to_string(),
        )
        .unwrap();
        assert_eq!(recover(&e.store, &e.paths, a).unwrap(), Recovery::Clean);
        assert_eq!(
            e.store.credential(a).unwrap().unwrap().oauth["refreshToken"],
            "refresh-a-renewed"
        );
        assert!(!state(&e.store, a).unwrap().held_back);
    }

    #[test]
    fn an_interrupted_check_holds_the_account_back_until_a_renewal_settles_it() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let interrupted = fake(Fake {
            timed_out: true,
            stdout: String::new(),
            ..Default::default()
        });
        check_with(&e.store, &e.paths, a, Trigger::BeforeSwap, &interrupted).unwrap();
        assert!(state(&e.store, a).unwrap().held_back);
        let account = e.store.account(a).unwrap().unwrap();
        assert!(!due_before_swap(&e.store, &account, time::now_ms()).unwrap());

        // The next check makes Claude Code renew (the access token is given as expired).
        let expiry = std::sync::Mutex::new(None);
        let settle = |dir: &Path| {
            let creds: Value = serde_json::from_str(
                &std::fs::read_to_string(dir.join(".credentials.json")).unwrap(),
            )
            .unwrap();
            *expiry.lock().unwrap() = creds["claudeAiOauth"]["expiresAt"].as_i64();
            fake(Fake {
                stamp: Some("ua".into()),
                renew: Some(Some(oauth("a-renewed"))),
                ..Default::default()
            })(dir)
        };
        let out = check_with(&e.store, &e.paths, a, Trigger::Background, &settle).unwrap();
        assert!(matches!(out, CheckOutcome::Read { .. }), "{out:?}");
        assert_eq!(expiry.into_inner().unwrap(), Some(0));
        assert!(!state(&e.store, a).unwrap().held_back);
    }

    #[test]
    fn never_checks_the_active_account_or_one_whose_login_is_in_use() {
        let e = env();
        let active = account(&e, "a", "ua", AccountState::Active);
        let parked = account(&e, "b", "ub", AccountState::Parked);
        let paused = account(&e, "c", "uc", AccountState::Paused);
        let never = |_: &Path| -> Result<Run> { panic!("Claude Code must not run") };
        for (id, trigger) in [(active, Trigger::Manual), (paused, Trigger::Background)] {
            assert!(matches!(
                check_with(&e.store, &e.paths, id, trigger, &never).unwrap(),
                CheckOutcome::Skipped { .. }
            ));
        }
        let swap_holds_it = DirLock::acquire(
            &e.paths.account_lock(parked),
            LockOptions::claude(Duration::ZERO),
        )
        .unwrap();
        assert!(matches!(
            check_with(&e.store, &e.paths, parked, Trigger::Background, &never).unwrap(),
            CheckOutcome::Skipped { .. }
        ));
        assert!(
            state(&e.store, parked).unwrap().last_attempt_at.is_some(),
            "counted, so the background moves on to another account"
        );
        drop(swap_holds_it);
        e.store.set_setting("usage_checks", Some("false")).unwrap();
        assert!(matches!(
            check_with(&e.store, &e.paths, parked, Trigger::BeforeSwap, &never).unwrap(),
            CheckOutcome::Skipped { .. }
        ));
    }

    #[test]
    fn recovery_follows_the_login_the_check_started_from() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let start = e.store.credential_meta(a).unwrap().unwrap().refresh_fp;
        let leave = |tag: &str, start_fp: &str, oauth: &Value| {
            let dir = e
                .paths
                .staging_root()
                .join(format!("{SCRATCH_PREFIX}{a}-1-{tag}"));
            credfile::write_oauth(&dir.join(".credentials.json"), oauth).unwrap();
            std::fs::write(dir.join(START_FP_FILE), start_fp).unwrap();
            dir
        };
        // Started from the stored login and renewed it: recovered.
        let renewed = leave("x", &start, &oauth("a-renewed"));
        recover(&e.store, &e.paths, a).unwrap();
        assert_eq!(
            e.store.credential(a).unwrap().unwrap().oauth["refreshToken"],
            "refresh-a-renewed"
        );
        assert!(!renewed.exists());
        // Started from a login stored before a re-login: stale, discarded.
        let stale = leave("y", &start, &oauth("a-older"));
        recover(&e.store, &e.paths, a).unwrap();
        assert_eq!(
            e.store.credential(a).unwrap().unwrap().oauth["refreshToken"],
            "refresh-a-renewed"
        );
        assert!(!stale.exists());
        // Its Claude Code still runs: left alone, and the account is busy.
        let live = leave("z", "whatever", &oauth("a-live"));
        std::fs::write(live.join(PID_FILE), std::process::id().to_string()).unwrap();
        assert!(matches!(
            recover(&e.store, &e.paths, a).unwrap(),
            Recovery::Busy(_)
        ));
        assert!(live.exists());
    }

    #[test]
    fn removing_an_account_cleans_up_after_its_checks() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        save_state(
            &e.store,
            a,
            &CheckState {
                problem: Some("x".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let dir = e
            .paths
            .staging_root()
            .join(format!("{SCRATCH_PREFIX}{a}-1-x"));
        std::fs::create_dir_all(&dir).unwrap();
        remove_account(&e.store, &e.paths, a).unwrap();
        assert!(e.store.account(a).unwrap().is_none());
        assert!(!dir.exists());
        assert_eq!(state(&e.store, a).unwrap(), CheckState::default());
    }

    #[test]
    fn confirming_the_active_account_cannot_renew_its_login() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Active);
        credfile::write_oauth(&e.paths.credentials_file(), &oauth("a-live")).unwrap();
        let given = std::sync::Mutex::new(None);
        let run = |dir: &Path| {
            let creds: Value = serde_json::from_str(
                &std::fs::read_to_string(dir.join(".credentials.json")).unwrap(),
            )
            .unwrap();
            *given.lock().unwrap() = Some(creds["claudeAiOauth"].clone());
            fake(Fake {
                stamp: Some("ua".into()),
                pct: 40.0,
                ..Default::default()
            })(dir)
        };
        let out = confirm_with(&e.store, &e.paths, &run).unwrap();
        assert!(matches!(out, CheckOutcome::Read { .. }), "{out:?}");
        let given = given.into_inner().unwrap().unwrap();
        assert_eq!(given["refreshToken"], "", "no refresh token, so no renewal");
        assert_eq!(given["accessToken"], "access-a-live");
        assert_eq!(
            e.store.last_sample(a).unwrap().unwrap().windows[0].pct,
            40.0
        );
        assert!(
            !swap::known_resets(&e.store, a, time::now_s())
                .unwrap()
                .is_empty()
        );
        assert_eq!(scratch_dirs_left(&e), 0);
        assert_eq!(
            e.store.credential(a).unwrap().unwrap().oauth["refreshToken"],
            "refresh-a",
            "the store is untouched"
        );
    }

    #[test]
    fn background_order_and_intervals() {
        let e = env();
        let now = time::now_ms();
        let first = account(&e, "a", "ua", AccountState::Parked);
        let second = account(&e, "b", "ub", AccountState::Parked);
        account(&e, "c", "uc", AccountState::Active);
        account(&e, "d", "ud", AccountState::Paused);
        assert_eq!(next_due(&e.store, &e.paths, now).unwrap(), Some(first));

        save_state(
            &e.store,
            first,
            &CheckState {
                last_attempt_at: Some(now - 60_000),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(next_due(&e.store, &e.paths, now).unwrap(), Some(second));

        e.store
            .record_sample(&Sample {
                ts: now - 30_000,
                account_id: second,
                profile: ".claude-b".into(),
                session_id: None,
                model: None,
                windows: vec![],
            })
            .unwrap();
        assert_eq!(
            next_due(&e.store, &e.paths, now).unwrap(),
            None,
            "a recent reading from anywhere counts"
        );
        let b = e.store.account(second).unwrap().unwrap();
        assert!(!due_before_swap(&e.store, &b, now).unwrap());
        assert!(due_before_swap(&e.store, &b, now + 3_600_000).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_timeout_kills_the_whole_process_tree() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude");
        std::fs::write(
            &claude,
            "#!/bin/sh\nsleep 30 &\necho $! > \"$CLAUDE_CONFIG_DIR/child.pid\"\nsleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let started = Instant::now();
        let run = run_claude(&claude, &scratch, Duration::from_secs(1)).unwrap();
        assert!(run.timed_out);
        assert!(started.elapsed() < Duration::from_secs(10));
        let child: u32 = std::fs::read_to_string(scratch.join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!process::alive(child), "its child was killed too");
    }

    #[cfg(windows)]
    #[test]
    fn a_timeout_kills_the_whole_process_tree() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude.cmd");
        std::fs::write(&claude, "@echo off\r\nping -n 30 127.0.0.1 >nul\r\n").unwrap();
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let started = Instant::now();
        let run = run_claude(&claude, &scratch, Duration::from_secs(1)).unwrap();
        assert!(run.timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "returned after {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn reasons_read_well() {
        assert_eq!(
            failure(
                &Ok(Run {
                    exit_code: Some(1),
                    stderr: "boom\nError: not logged in\n".into(),
                    ..Default::default()
                }),
                ""
            ),
            "Claude Code exited with 1: Error: not logged in"
        );
        assert_eq!(
            failure(
                &Ok(Run {
                    exit_code: Some(0),
                    ..Default::default()
                }),
                "[ERROR] Failed to load usage data: Request failed with status code 500\n"
            ),
            "Claude Code could not read the usage: Request failed with status code 500"
        );
        assert_eq!(rate_limited("all good"), None);
        assert_eq!(
            rate_limited("[DEBUG] some other endpoint: status code 429"),
            None,
            "only the usage endpoint's refusals count"
        );
        assert_eq!(
            rate_limited("[ERROR] Failed to load usage data: Request failed with status code 429"),
            Some(DEFAULT_RETRY_AFTER_MS)
        );
        assert_eq!(ran_as_prompt(USAGE_RESULT), None);
        assert!(ran_as_prompt(r#"{"num_turns":2}"#).is_some());
        assert_eq!(ran_as_prompt(""), None, "no result to judge");
        assert_eq!(
            describe(&CheckOutcome::Read {
                windows: vec![SampleWindow {
                    name: "five_hour".into(),
                    pct: 100.0,
                    resets_at: None
                }]
            }),
            "5-hour 100%"
        );
    }
}
