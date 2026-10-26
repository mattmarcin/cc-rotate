//! Usage checks (spec section 20): reading an account's usage without a turn, by running Claude
//! Code's own `/usage` with the account's login in a scratch profile.
//!
//! `claude -p /usage` asks `GET /api/oauth/usage` for the account's usage and caches the answer
//! in the scratch profile's `.claude.json`, stamped with the account it fetched for, whose
//! identity Claude Code fetches itself (F17). It makes no model request and uses none of the
//! account's quota; Claude Code's JSON result says so (`num_turns` 0, `local_command` `usage`),
//! and a result saying otherwise stops all checks. cc-rotate itself sends nothing.
//!
//! A warm-up (spec section 23, `warm_up`) is the one exception, for accounts the user opted in
//! (`Account::keep_running`): in the same scratch profile and under the same lock, before its
//! `/usage`, Claude Code sends one short prompt to the smallest model with no tools, which
//! starts the account's next 5-hour window. A result showing another model, more than one turn
//! or a larger cost stops warm-ups until the user turns them on again.
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
/// The longest a check or warm-up holds an account's lock while Claude Code runs: a warm-up's
/// prompt, then its check.
pub const JOB_TIMEOUT: Duration = Duration::from_secs(2 * 60);
/// Before a swap, a check may not hold the swap up for long.
const BEFORE_SWAP_TIMEOUT: Duration = Duration::from_secs(30);
/// The wait after a 429 when Claude Code did not log one.
const DEFAULT_RETRY_AFTER_MS: i64 = 5 * 60_000;
/// Claude Code never holds a 429 longer than this, and neither does cc-rotate.
const MAX_RETRY_AFTER_MS: i64 = 3_600_000;
/// A held-back account is checked again at most this often.
const HELD_BACK_RETRY_MS: i64 = 5 * 60_000;
/// How long an unreadable leftover is kept in case it becomes readable (20.3).
const LEFTOVER_KEPT_MS: i64 = 3_600_000;
/// The warm-up's prompt and model: one word back from the smallest model, with no tools.
const WARM_PROMPT: &str = "Reply with the single word OK.";
const WARM_MODEL: &str = "haiku";
/// A warm-up that costs more than this (US dollars at API prices, as Claude Code reports it),
/// takes more than one turn or uses another model did not do what was asked, and stops
/// warm-ups. One measured under a tenth of a cent.
const WARM_MAX_COST_USD: f64 = 0.02;
/// After a warm-up whose prompt did not go through, the next one waits this long.
const WARM_RETRY_MS: i64 = 15 * 60_000;
/// A warm-up's problem is shown this long after its last try: past a window's length, the
/// account has likely been used since.
pub const WARM_PROBLEM_SHOWN_MS: i64 = 6 * 3_600_000;
/// A 5-hour window starts at its first request, rounded down to 10 minutes.
const WINDOW_START_STEP_MS: i64 = 10 * 60_000;
const FIVE_HOURS_MS: i64 = 5 * 3_600_000;
/// `CLAUDE_CODE_*` variables kept for the check: how Claude Code reaches the network and finds
/// Git Bash on Windows, which it needs to start.
const KEPT_ENV: &[&str] = &[
    "CLAUDE_CODE_GIT_BASH_PATH",
    "CLAUDE_CODE_CERT_STORE",
    "CLAUDE_CODE_CLIENT_CERT",
    "CLAUDE_CODE_CLIENT_KEY",
    "CLAUDE_CODE_CLIENT_KEY_PASSPHRASE",
    "CLAUDE_CODE_HTTP_PROXY",
    "CLAUDE_CODE_HTTPS_PROXY",
];
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
/// What a warm-up's prompt run leaves, next to its check's files.
const WARM_OUT_FILE: &str = "warm-out.json";
const WARM_ERR_FILE: &str = "warm-err.log";
const WARM_DEBUG_FILE: &str = "warm-debug.log";
/// Prefix of the meta keys holding each account's `CheckState`.
const META_STATE_PREFIX: &str = "usage_check:";
/// Set (to the reason) when checks must not run until the user turns them on again: Claude Code
/// ran `/usage` as a prompt, or keeps logins where cc-rotate cannot read them. Setting
/// `usage_checks` clears it (`Store::set_setting`).
pub const META_BLOCKED: &str = "usage_checks_blocked";
/// Set (to the reason) when a warm-up did something other than asked. Warm-ups stay stopped
/// until the user turns keeping a window running on for an account again
/// (`Store::set_keep_running`).
pub const META_WARM_BLOCKED: &str = "warm_ups_blocked";
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
    /// The app keeps an account's 5-hour window running (`warm_up`).
    WarmUp,
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
    /// When a warm-up's prompt last went through (epoch ms).
    #[serde(default)]
    pub warmed_at: Option<i64>,
    /// When the last warm-up started (epoch ms).
    #[serde(default)]
    pub warm_attempt_at: Option<i64>,
    /// Why the last warm-up's prompt did not go through.
    #[serde(default)]
    pub warm_problem: Option<String>,
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

/// What a warm-up did.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WarmOutcome {
    /// The prompt went through, so the account's 5-hour window runs; `check` is the reading
    /// right after it.
    Sent { cost_usd: f64, check: CheckOutcome },
    /// Nothing was sent.
    Skipped { why: String },
    /// The prompt did not go through; `check` is the reading after it.
    Failed { why: String, check: CheckOutcome },
    /// The prompt went through but did more than asked, which stopped warm-ups.
    Unexpected { why: String, check: CheckOutcome },
}

/// What a warm-up's prompt run did.
#[derive(Debug, Clone, PartialEq)]
enum Prompted {
    /// Claude Code sent the prompt and got its answer.
    Sent { cost_usd: f64 },
    /// It did not go through: the account at a limit, a network or login problem.
    Failed { why: String },
    /// It went through, but not as asked: another model, more turns or a larger cost.
    Unexpected { why: String },
}

/// Runs Claude Code in a scratch profile (`run_claude`, `run_prompt`, or a fake in tests).
type Runner<'a> = &'a dyn Fn(&Path) -> Result<Run>;

/// What a run of Claude Code left behind.
#[derive(Debug, Default)]
struct Run {
    exit_code: Option<i32>,
    /// Killed before it finished: at the time limit, or because waiting for it failed.
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

/// Whether a login that changed in the profile is identified with Claude Code before it is
/// stored (`swap::identify_changed_login`): a routine run of Claude Code like a check, so only
/// while checks are on and possible, and while the profile's login can be read in full (on
/// macOS, not while the Keychain cannot be used). Otherwise it is stored at once, as before
/// logins were identified.
pub fn identifies_logins(store: &Store, paths: &Paths) -> Result<bool> {
    Ok(store.settings()?.usage_checks
        && checks_possible(store, paths)?.is_none()
        && paths.credentials().complete().unwrap_or(false))
}

/// Whether nothing at all keeps automatic checks from running.
fn checks_possible(store: &Store, paths: &Paths) -> Result<Option<String>> {
    if let Some(reason) = blocked(store)? {
        return Ok(Some(format!("usage checks are stopped because {reason}")));
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
            "usage checks don't support Windows Credential Manager, where Claude Code keeps logins on this computer".into(),
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
    match next_held_back(store, paths, now_ms)? {
        Some(id) => Ok(Some(id)),
        None => next_routine(store, paths, now_ms),
    }
}

/// The first parked account in pick order whose reading and last check are both older than
/// `usage_check_interval_s`, and whose back-off is over.
fn next_routine(store: &Store, paths: &Paths, now_ms: i64) -> Result<Option<i64>> {
    if !checks_on(store, paths)? {
        return Ok(None);
    }
    let older_than = now_ms - store.settings()?.usage_check_interval_s * 1000;
    for c in parked_in_order(store, now_ms)? {
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

/// What the background runs next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    Check(i64),
    WarmUp(i64),
}

/// The next background job: a held-back account's check first (it is out of use until
/// settled), then a warm-up (a window should start as soon as the last one ended), then the
/// most overdue check (`next_due`).
pub fn next_job(store: &Store, paths: &Paths, now_ms: i64) -> Result<Option<Due>> {
    if let Some(id) = next_held_back(store, paths, now_ms)? {
        return Ok(Some(Due::Check(id)));
    }
    if let Some(id) = next_warm_up(store, paths, now_ms)? {
        return Ok(Some(Due::WarmUp(id)));
    }
    Ok(next_routine(store, paths, now_ms)?.map(Due::Check))
}

/// Whether automatic checks (and so warm-ups) run at all.
fn checks_on(store: &Store, paths: &Paths) -> Result<bool> {
    Ok(store.settings()?.usage_checks && checks_possible(store, paths)?.is_none())
}

/// The parked accounts, in the order the scheduler would use them.
fn parked_in_order(store: &Store, now_ms: i64) -> Result<Vec<scheduler::Candidate>> {
    let (pool, policy) = swap::load_pool(store, now_ms / 1000)?;
    Ok(scheduler::ranking(&pool, policy, now_ms / 1000)
        .iter()
        .filter_map(|r| pool.iter().find(|c| c.account.id == r.account_id))
        .filter(|c| c.account.state == AccountState::Parked)
        .cloned()
        .collect())
}

/// A held-back account not tried in the last few minutes, whose back-off is over.
fn next_held_back(store: &Store, paths: &Paths, now_ms: i64) -> Result<Option<i64>> {
    if !checks_on(store, paths)? {
        return Ok(None);
    }
    for c in parked_in_order(store, now_ms)? {
        let st = state(store, c.account.id)?;
        let waiting = st.not_before.is_some_and(|t| t > now_ms);
        if st.held_back
            && !waiting
            && st
                .last_attempt_at
                .is_none_or(|t| now_ms - t > HELD_BACK_RETRY_MS)
        {
            return Ok(Some(c.account.id));
        }
    }
    Ok(None)
}

/// When a 5-hour window started by a request at `at_ms` resets.
pub fn window_end_ms(at_ms: i64) -> i64 {
    at_ms - at_ms.rem_euclid(WINDOW_START_STEP_MS) + FIVE_HOURS_MS
}

/// The account whose 5-hour window the background should start now (spec section 23): a parked
/// account the user opted in (`Account::keep_running`), not held back, kept out of use by no
/// other limit, with no 5-hour window running by its latest reading nor started by a warm-up,
/// and no warm-up tried in the last `WARM_RETRY_MS`. Warm-ups run while usage checks do.
pub fn next_warm_up(store: &Store, paths: &Paths, now_ms: i64) -> Result<Option<i64>> {
    if !checks_on(store, paths)? || warm_ups_blocked(store)?.is_some() {
        return Ok(None);
    }
    let now_s = now_ms / 1000;
    for c in parked_in_order(store, now_ms)? {
        if !c.account.keep_running || c.held_back {
            continue;
        }
        // Another limit keeps it out of use: a window would only run unused.
        let other_limit = c.windows.iter().any(|w| {
            w.blocking() && !(w.kind == scheduler::WindowKind::Session && w.model.is_none())
        });
        if other_limit || c.session_running(now_s) {
            continue;
        }
        let st = state(store, c.account.id)?;
        // A window a warm-up started runs whether or not the reading after it came through; a
        // reading since says when it really ends (the prompt may have landed in a window that
        // was already running).
        // Only a reading that shows a window running since the warm-up counts: one without it
        // may lag behind the request.
        let read_since = |t: i64| {
            store.last_sample(c.account.id).map(|s| {
                s.is_some_and(|s| {
                    s.ts > t
                        && s.windows.iter().any(|w| {
                            w.name == scheduler::FIVE_HOUR
                                && w.resets_at.is_some_and(|r| r * 1000 > t)
                        })
                })
            })
        };
        let started = match st.warmed_at {
            Some(t) => window_end_ms(t) > now_ms && !read_since(t)?,
            None => false,
        };
        let tried = st
            .warm_attempt_at
            .is_some_and(|t| now_ms - t < WARM_RETRY_MS);
        if !started && !tried {
            return Ok(Some(c.account.id));
        }
    }
    Ok(None)
}

/// Why warm-ups are stopped until the user turns them on again, if they are.
pub fn warm_ups_blocked(store: &Store) -> Result<Option<String>> {
    store.meta(META_WARM_BLOCKED)
}

/// Why warm-ups do not run now, if they don't: they were stopped, or usage checks are off or
/// stopped.
pub fn warm_ups_paused(store: &Store, paths: &Paths) -> Result<Option<String>> {
    if let Some(why) = warm_ups_blocked(store)? {
        return Ok(Some(format!("they stopped because {why}")));
    }
    if !store.settings()?.usage_checks {
        return Ok(Some("they need usage checks, which are off".into()));
    }
    checks_possible(store, paths)
}

/// Start the account's next 5-hour window (spec section 23): Claude Code sends a short prompt to
/// the smallest model with the account's login, then reads its usage like a check, so the new
/// window shows. Everything a check guarantees about the login holds for both runs.
pub fn warm_up(
    store: &Store,
    paths: &Paths,
    account_id: i64,
    trigger: Trigger,
) -> Result<WarmOutcome> {
    if trigger != Trigger::Manual {
        if let Some(why) = warm_ups_blocked(store)? {
            return Ok(WarmOutcome::Skipped {
                why: format!("keeping windows running is stopped because {why}"),
            });
        }
    }
    // Found by each run, so a missing `claude` is a warm-up that failed (and is retried later),
    // not an error before anything is recorded.
    warm_up_with(
        store,
        paths,
        account_id,
        trigger,
        &|scratch| run_prompt(&login::claude_executable(store)?, scratch, RUN_TIMEOUT),
        &|scratch| run_claude(&login::claude_executable(store)?, scratch, RUN_TIMEOUT),
    )
}

fn warm_up_with(
    store: &Store,
    paths: &Paths,
    account_id: i64,
    trigger: Trigger,
    prompt: &dyn Fn(&Path) -> Result<Run>,
    run: &dyn Fn(&Path) -> Result<Run>,
) -> Result<WarmOutcome> {
    let mut verdict = None;
    let check = job(
        store,
        paths,
        account_id,
        trigger,
        Some(prompt),
        run,
        &mut verdict,
    );
    if verdict.is_none() {
        // Nothing was sent (the account was busy, its login unreadable, a swap unfinished...):
        // count it as a try, so the background goes on to other work instead of retrying this
        // every minute.
        let mut st = state(store, account_id)?;
        st.warm_attempt_at = Some(time::now_ms());
        save_state(store, account_id, &st)?;
    }
    let check = check?;
    Ok(match verdict {
        Some(Prompted::Sent { cost_usd }) => WarmOutcome::Sent { cost_usd, check },
        Some(Prompted::Failed { why }) => WarmOutcome::Failed { why, check },
        Some(Prompted::Unexpected { why }) => WarmOutcome::Unexpected { why, check },
        None => WarmOutcome::Skipped {
            why: match check {
                CheckOutcome::Skipped { why } | CheckOutcome::Failed { why } => why,
                other => describe(&other),
            },
        },
    })
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
    job(store, paths, account_id, trigger, None, run, &mut None)
}

/// A check, after a warm-up's prompt when `prompt` is given (its verdict goes to
/// `verdict_out`): both run under the account's lock in one scratch profile, so a renewal by
/// either is handled as a check's is.
fn job(
    store: &Store,
    paths: &Paths,
    account_id: i64,
    trigger: Trigger,
    prompt: Option<Runner>,
    run: Runner,
    verdict_out: &mut Option<Prompted>,
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
    if matches!(trigger, Trigger::Background | Trigger::BeforeSwap)
        && st.not_before.is_some_and(|t| t > now)
    {
        return skipped("Anthropic asked to wait before the next check");
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
        AccountState::Active => {
            return skipped("it's in use, so its usage comes from the status line");
        }
        AccountState::NeedsLogin => return skipped("it's signed out"),
        AccountState::Paused if trigger != Trigger::Manual => return skipped("it's paused"),
        _ => {}
    }
    if trigger == Trigger::WarmUp && !account.keep_running {
        return skipped("keeping its 5-hour window running is off");
    }
    st = state(store, account_id)?;
    // A warm-up's prompt does not ask the usage endpoint, so the endpoint's wait, or the
    // back-off after a failed reading, only holds up the reading after it, which is left out.
    // From the state read under the lock, so a check that just stored one is seen.
    let usage_wait = st
        .not_before
        .filter(|&t| prompt.is_some() && trigger != Trigger::Manual && t > now);
    let stored = match store.credential(account_id) {
        Ok(Some(c)) => c,
        // Counted as a try, so a held-back account without a login is not picked every minute.
        Ok(None) => return attempted(store, "it has no saved login"),
        Err(e) => {
            let why = format!("its saved login can't be read: {e:#}");
            st.last_attempt_at = Some(time::now_ms());
            st.problem = Some(why.clone());
            save_state(store, account_id, &st)?;
            return Ok(CheckOutcome::Failed { why });
        }
    };

    // Not while a swap is pending (running, or stopped half way): which account the profile's
    // login is, and so which stored logins are current, is not settled.
    if swap::pending_swap(store)?.is_some() {
        return attempted(store, "a swap isn't finished yet");
    }
    // Never with the login the profile's sessions are using (a swap that stopped half way can
    // leave a parked account's login there): whichever renewed it first would sign the other out.
    let in_profile = paths
        .credentials()
        .read_oauth()
        .ok()
        .flatten()
        .as_ref()
        .and_then(credfile::oauth_fingerprint);
    if in_profile.as_deref() == Some(stored.refresh_fp.as_str()) {
        return attempted(store, "the cc-rotate profile is using its login");
    }

    let started = time::now_ms();
    st.last_attempt_at = Some(started);
    if prompt.is_some() {
        st.warm_attempt_at = Some(started);
    }
    save_state(store, account_id, &st)?;

    let mut scratch = Scratch::create(paths, SCRATCH_PREFIX, Some((account_id, started)))?;
    let mut oauth = stored.oauth.clone();
    if st.held_back {
        // Settle whether the stored login still works: make Claude Code renew it.
        oauth["expiresAt"] = json!(0);
    }
    // First, so a scratch profile without it is known never to have held the login (20.3).
    std::fs::write(scratch.dir.join(START_FP_FILE), &stored.refresh_fp)?;
    credfile::write_oauth(&scratch.dir.join(".credentials.json"), &oauth)?;

    // From here on Claude Code may renew the login: keep the scratch profile until the outcome
    // is stored.
    scratch.keep = true;
    let warm = prompt.map(|p| p(&scratch.dir));
    let verdict = warm.as_ref().map(judge_warm_up);
    verdict_out.clone_from(&verdict);
    // Only a reading `/usage` fetched counts, not one the prompt run may have cached.
    let usage_started = time::now_ms();
    let ran = match usage_wait {
        Some(_) => Ok(Run::default()),
        None => run(&scratch.dir),
    };

    let left = match Credentials::for_profile(&scratch.dir).read_oauth() {
        Ok(left) => left,
        Err(e) => {
            // Claude Code may have renewed the login and the only copy is in there: keep it for
            // `recover`, and keep the account out of swaps until then.
            let why = format!(
                "can't read the login Claude Code left after the check ({e:#}). On hold until a check succeeds."
            );
            st.held_back = true;
            st.problem = Some(why.clone());
            note_verdict(store, &mut st, verdict.as_ref(), account_id, usage_started)?;
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
        .filter(|r| r.fetched_at_ms >= usage_started - 1000);
    // Kept with the reading when the check reads the account's usage.
    let extra_usage = report
        .as_ref()
        .and_then(|r| Some((r.extra_usage.clone()?, r.fetched_at_ms)));
    let debug_log = std::fs::read_to_string(scratch.dir.join(DEBUG_FILE)).unwrap_or_default();
    let warm_refused = warm.as_ref().and_then(|_| {
        rate_limited(
            &std::fs::read_to_string(scratch.dir.join(WARM_DEBUG_FILE)).unwrap_or_default(),
        )
    });
    let credman = cfg!(windows) && credstore::windows_credman_enabled(&scratch.dir, None);
    let timed_out = ran.as_ref().is_ok_and(|r| r.timed_out)
        || warm
            .as_ref()
            .is_some_and(|w| w.as_ref().is_ok_and(|r| r.timed_out));
    let prompted = ran.as_ref().ok().and_then(|r| ran_as_prompt(&r.stdout));

    // No login left at all: Claude Code removed the one it was given, so where a renewal went
    // is unknown.
    let vanished = left.is_none();
    let dead = left.as_ref().is_some_and(credfile::is_dead);
    let renewal = left
        .filter(|o| !credfile::is_dead(o))
        .filter(|o| credfile::oauth_fingerprint(o).as_deref() != Some(stored.refresh_fp.as_str()));
    let mut wrong_account = None;
    // The report was checked against the account's identity, so its reset times are its own.
    let mut vouched = false;

    let outcome = if let Some(why) = &prompted {
        CheckOutcome::Failed { why: why.clone() }
    } else if dead {
        CheckOutcome::LoginRejected
    } else if let Some(until) = usage_wait {
        CheckOutcome::Skipped {
            why: format!(
                "the reading waits until {}",
                time::local(until / 1000, "%H:%M")
            ),
        }
    } else if let Some(report) = report {
        match (&account.account_uuid, &report.account_uuid) {
            (Some(expected), Some(got)) if expected != got => {
                wrong_account = Some(got.clone());
                CheckOutcome::Failed {
                    why: "the saved login belongs to a different account. Sign in to this account again."
                        .into(),
                }
            }
            (Some(_), None) => CheckOutcome::Failed {
                why: "Claude Code didn't say which account the usage is for".into(),
            },
            _ => {
                vouched = account.account_uuid.is_some();
                CheckOutcome::Read {
                    windows: report.windows,
                }
            }
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
    if (timed_out || credman || vanished) && !settled {
        st.held_back = true;
    }

    let now = time::now_ms();
    store.immediate(|s| {
        // `adopt_manual_login` changes an account without its lock (a `/login` in the profile):
        // when its login or state changed meanwhile, this check's verdict on the old login is
        // moot. Its reading of the account's usage still counts.
        let unchanged = s
            .credential_meta(account_id)?
            .is_some_and(|m| m.refresh_fp == stored.refresh_fp)
            && s.account(account_id)?.is_some_and(|a| {
                matches!(a.state, AccountState::Parked | AccountState::Paused)
            });
        let (renewal, dead, wrong_account) = if unchanged {
            (renewal.clone(), dead, wrong_account.clone())
        } else {
            (None, false, None)
        };
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
        if wrong_account.is_some() {
            s.set_state(account_id, AccountState::NeedsLogin)?;
            s.log_event(
                kind::ERROR,
                Some(account_id),
                None,
                Some("usage_check"),
                Some(json!({ "message": format!("{label}'s saved login belongs to a different account. Sign in to {label} again.", label = account.label) })),
            )?;
        }
        if let Some(why) = &prompted {
            block(s, why)?;
        }
        if credman {
            block(
                s,
                "Claude Code started keeping logins in Windows Credential Manager, which usage checks don't support",
            )?;
        }
        // After the prompt returned: a window it started may start that late, never earlier.
        note_verdict(s, &mut st, verdict.as_ref(), account_id, usage_started)?;
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
                if let Some((extra, read_at)) = &extra_usage {
                    s.set_extra_usage(account_id, extra, *read_at)?;
                }
                st.last_read_at = Some(now);
                st.not_before = None;
                st.problem = None;
            }
            // Routine: the endpoint allows a reading now and then, not a problem to report.
            CheckOutcome::RateLimited { until } => st.not_before = Some(*until),
            CheckOutcome::Failed { why } => {
                // A held-back account is tried again soon: it is out of use until settled.
                let back_off = if st.held_back {
                    HELD_BACK_RETRY_MS
                } else {
                    s.settings()?.usage_check_interval_s * 1000
                };
                st.not_before = Some(now + back_off);
                st.problem = Some(why.clone());
            }
            CheckOutcome::LoginRejected => {
                st.problem = Some("Anthropic rejected the saved login".into());
            }
            CheckOutcome::Skipped { .. } => {}
        }
        if let Some(wait) = warm_refused {
            // The prompt run asked the usage endpoint after all and was refused: wait as asked.
            st.not_before = st.not_before.max(Some(now + wait));
        }
        if st.held_back && st.problem.is_none() {
            st.problem = Some(if timed_out {
                "a check was interrupted, possibly while Claude Code was renewing the login. On hold until a check confirms it.".into()
            } else {
                "on hold until a check confirms its login still works".into()
            });
        }
        save_state(s, account_id, &st)
    })?;
    // The check is done: its scratch profile goes whatever follows.
    scratch.keep = false;
    if let (true, CheckOutcome::Read { windows }) = (vouched, &outcome) {
        // So that once the account is in use, a session still showing the account it used
        // before is told apart by its reset times (`swap::attribute`).
        swap::note_known_resets(store, account_id, windows, now / 1000)?;
    }
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
        Some(json!({ "message": format!("Usage checks stopped because {reason}. After updating Claude Code, turn usage checks off and on again in Settings, or run `cc-rotate-ctl config usage_checks true`.") })),
    )?;
    Ok(())
}

/// Record what a warm-up's prompt did: that it went through (`warmed_at`, a `warm_up` event),
/// why it did not, or, when it did more than asked, stop warm-ups.
fn note_verdict(
    store: &Store,
    st: &mut CheckState,
    verdict: Option<&Prompted>,
    account_id: i64,
    started: i64,
) -> Result<()> {
    match verdict {
        Some(Prompted::Sent { cost_usd }) => {
            st.warmed_at = Some(started);
            st.warm_problem = None;
            store.log_event(
                kind::WARM_UP,
                Some(account_id),
                None,
                Some("keep_running"),
                Some(json!({ "cost_usd": cost_usd })),
            )?;
        }
        Some(Prompted::Failed { why }) => st.warm_problem = Some(why.clone()),
        Some(Prompted::Unexpected { why }) => {
            // It sent something, so a window runs all the same; the stop says why.
            st.warmed_at = Some(started);
            st.warm_problem = None;
            block_warm_ups(store, why)?;
        }
        None => {}
    }
    Ok(())
}

/// Stop warm-ups until the user turns keeping a window running on again, and say why.
fn block_warm_ups(store: &Store, reason: &str) -> Result<()> {
    if warm_ups_blocked(store)?.is_some() {
        return Ok(());
    }
    store.set_meta(META_WARM_BLOCKED, reason)?;
    store.log_event(
        kind::ERROR,
        None,
        None,
        Some("warm_up"),
        Some(json!({ "message": format!("Keeping 5-hour windows running stopped because {reason}. Click Start again in an account's More dialog, or run `cc-rotate-ctl keep-running <label> yes`.") })),
    )?;
    Ok(())
}

/// What a warm-up's prompt run did, from Claude Code's JSON result. Anything that shows more
/// than the one short Haiku turn asked for counts as unexpected, even alongside an error.
fn judge_warm_up(ran: &Result<Run>) -> Prompted {
    let run = match ran {
        Ok(run) => run,
        Err(e) => {
            return Prompted::Failed {
                why: format!("{e:#}"),
            };
        }
    };
    let short = |text: &str| text.trim().chars().take(200).collect::<String>();
    let Ok(result) = serde_json::from_str::<Value>(run.stdout.trim()) else {
        let why = if run.timed_out {
            "Claude Code didn't finish in time".to_string()
        } else {
            short(
                run.stderr
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("Claude Code gave no result"),
            )
        };
        return Prompted::Failed { why };
    };
    let cost = result
        .get("total_cost_usd")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let turns = result.get("num_turns").and_then(Value::as_i64).unwrap_or(0);
    let other_model = result
        .get("modelUsage")
        .and_then(Value::as_object)
        .and_then(|m| {
            m.keys()
                .find(|k| !k.to_ascii_lowercase().contains(WARM_MODEL))
                .cloned()
        });
    if let Some(model) = other_model {
        return Prompted::Unexpected {
            why: format!("a warm-up used {model} instead of Haiku"),
        };
    }
    if turns > 1 {
        return Prompted::Unexpected {
            why: format!("a warm-up took {turns} turns instead of one"),
        };
    }
    if cost > WARM_MAX_COST_USD {
        return Prompted::Unexpected {
            why: format!(
                "a warm-up cost ${cost:.4} at API prices, more than the ${WARM_MAX_COST_USD} expected"
            ),
        };
    }
    let is_error = result
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if is_error || turns == 0 {
        let why = result
            .get("result")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .unwrap_or("Claude Code reported an error");
        return Prompted::Failed { why: short(why) };
    }
    Prompted::Sent { cost_usd: cost }
}

/// Why a result shows that Claude Code ran the text as a prompt (a model request, using the
/// account's quota) instead of its `/usage` command, which no check may ever do again. Only a
/// turn or a cost counts as evidence; an error result or a field Claude Code renamed does not.
fn ran_as_prompt(stdout: &str) -> Option<String> {
    let result: Value = serde_json::from_str(stdout.trim()).ok()?;
    let turns = result.get("num_turns").and_then(Value::as_i64).unwrap_or(0);
    let cost = result
        .get("total_cost_usd")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let output = result
        .pointer("/usage/output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    (turns > 0 || cost > 0.0 || output > 0).then(|| {
        "Claude Code sent /usage to the model as a prompt instead of running its command, which uses the account's limits".to_string()
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

/// Settle what earlier checks of this account that did not finish left in their scratch
/// profiles. Call with the account's lock held.
///
/// A leftover means the check never stored its outcome, so Claude Code may have renewed the
/// login, retiring the stored one, without that being recorded. One from a check that started
/// from a login stored before the one stored now (a re-login since) is stale and discarded
/// unread. Otherwise a renewal in it is stored, a rejected login marks the account, and anything
/// else (the login unchanged, none at all) leaves the doubt: the account is held back, and its
/// next check makes Claude Code renew to settle it. A leftover whose login cannot be read is kept
/// (the account counts as busy) for `LEFTOVER_KEPT_MS`, in case it becomes readable, then given
/// up on the same way.
pub fn recover(store: &Store, paths: &Paths, account_id: i64) -> Result<Recovery> {
    let prefix = format!("{SCRATCH_PREFIX}{account_id}-");
    for dir in scratch_dirs(paths, &prefix) {
        if running(&dir) {
            return Ok(Recovery::Busy(
                "a usage check of this account is still running".into(),
            ));
        }
        let Some(stored) = store.credential_meta(account_id)? else {
            remove_scratch(&dir);
            continue;
        };
        // Written before the login itself: without it, Claude Code never got the login.
        let start = std::fs::read_to_string(dir.join(START_FP_FILE))
            .ok()
            .map(|s| s.trim().to_string());
        if start.as_deref() != Some(stored.refresh_fp.as_str()) {
            remove_scratch(&dir);
            continue;
        }
        match Credentials::for_profile(&dir).read_oauth() {
            Ok(Some(oauth)) if credfile::is_dead(&oauth) => {
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
            }
            Ok(Some(oauth))
                if credfile::oauth_fingerprint(&oauth).as_deref()
                    != Some(stored.refresh_fp.as_str()) =>
            {
                store.immediate(|s| {
                    s.save_credential(account_id, &oauth, SOURCE)?;
                    s.log_event(kind::SYNC, Some(account_id), None, Some("recovered"), None)?;
                    let mut st = state(s, account_id)?;
                    st.held_back = false;
                    save_state(s, account_id, &st)
                })?;
            }
            Ok(_) => hold_back(store, account_id)?,
            Err(_) => {
                hold_back(store, account_id)?;
                if time::now_ms() - started_at(&dir).unwrap_or(0) < LEFTOVER_KEPT_MS {
                    return Ok(Recovery::Busy(
                        "the login an earlier usage check left can't be read yet".into(),
                    ));
                }
            }
        }
        remove_scratch(&dir);
    }
    Ok(Recovery::Clean)
}

/// When a check's scratch profile was created (from its name).
fn started_at(dir: &Path) -> Option<i64> {
    dir.file_name()?
        .to_str()?
        .strip_prefix(SCRATCH_PREFIX)?
        .split('-')
        .nth(1)?
        .parse()
        .ok()
}

/// Delete what earlier checks of this account left (after a fresh login supersedes the login
/// they ran with). Call with the account's lock held.
pub fn discard_leftovers(paths: &Paths, account_id: i64) {
    for dir in scratch_dirs(paths, &format!("{SCRATCH_PREFIX}{account_id}-")) {
        if !running(&dir) {
            remove_scratch(&dir);
        }
    }
}

/// Keep an account out of use until a check settles whether its login still works.
pub(crate) fn hold_back(store: &Store, account_id: i64) -> Result<()> {
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
    let wait = Duration::from_millis(store.settings()?.lock_timeout_ms) + JOB_TIMEOUT;
    let _lock = DirLock::acquire(&paths.account_lock(account_id), LockOptions::claude(wait))
        .context("waiting for a usage check of the account to finish")?;
    let dirs = scratch_dirs(paths, &format!("{SCRATCH_PREFIX}{account_id}-"));
    if dirs.iter().any(|d| running(d)) {
        bail!("A usage check of this account is running. Try again in a minute.");
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
        return skipped("its login is about to be renewed");
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
            why: "the profile's login didn't report the usage of the account in use".into(),
        });
    }
    let now = time::now_ms();
    if let Some(extra) = &report.extra_usage {
        store.set_extra_usage(active.id, extra, report.fetched_at_ms)?;
    }
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

/// The account a login belongs to, as Claude Code reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Who {
    pub uuid: Option<String>,
    pub email: Option<String>,
    pub org_uuid: Option<String>,
}

impl Who {
    /// Whether this is `account`: by account id, or else by email. The same person can be a
    /// member of several organizations with one account id and email, so the organization must
    /// match too where both are known. `None` when neither the id nor the email can be compared.
    pub fn is(&self, account: &Account) -> Option<bool> {
        let person = match (
            &account.account_uuid,
            &self.uuid,
            &account.email,
            &self.email,
        ) {
            (Some(x), Some(y), _, _) => x == y,
            (_, _, Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
            _ => return None,
        };
        let org = match (&account.org_uuid, &self.org_uuid) {
            (Some(x), Some(y)) => x == y,
            _ => true,
        };
        Some(person && org)
    }
}

/// Which account the rotation profile's login belongs to, as Claude Code reports it: run with a
/// copy of it whose refresh token is removed (so Claude Code cannot renew it), and read the
/// identity Claude Code fetched for it. `None` when that cannot be told now (no login, an access
/// token about to expire, Claude Code failing).
pub fn identify_profile_login(store: &Store, paths: &Paths) -> Result<Option<Who>> {
    identify_with(store, paths, &|scratch| {
        run_claude(
            &login::claude_executable(store)?,
            scratch,
            BEFORE_SWAP_TIMEOUT,
        )
    })
}

fn identify_with(
    store: &Store,
    paths: &Paths,
    run: &dyn Fn(&Path) -> Result<Run>,
) -> Result<Option<Who>> {
    if blocked(store)?.is_some() {
        return Ok(None);
    }
    let Some(mut oauth) = paths.credentials().read_oauth()? else {
        return Ok(None);
    };
    let expires = oauth.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    if credfile::is_dead(&oauth) || expires < time::now_ms() + 120_000 {
        return Ok(None);
    }
    oauth["refreshToken"] = json!("");
    let scratch = Scratch::create(paths, CONFIRM_PREFIX, None)?;
    credfile::write_oauth(&scratch.dir.join(".credentials.json"), &oauth)?;
    let ran = run(&scratch.dir);
    if let Some(why) = ran.as_ref().ok().and_then(|r| ran_as_prompt(&r.stdout)) {
        block(store, &why)?;
        return Ok(None);
    }
    Ok(credfile::read_document(&scratch.dir.join(".claude.json"))
        .ok()
        .flatten()
        .and_then(|doc| {
            let account = doc.get("oauthAccount")?;
            let text = |k: &str| {
                account
                    .get(k)
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
            };
            let who = Who {
                uuid: text("accountUuid"),
                email: text("emailAddress"),
                org_uuid: text("organizationUuid"),
            };
            (who.uuid.is_some() || who.email.is_some()).then_some(who)
        }))
}

/// Whether the Claude Code a scratch profile records as started is still running: a process
/// with that id and the same start time (ids are reused).
fn running(dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join(PID_FILE)) else {
        return false;
    };
    let mut parts = text.trim().splitn(2, ' ');
    let Some(pid) = parts.next().and_then(|p| p.parse::<u32>().ok()) else {
        return false;
    };
    let started = parts.next().unwrap_or_default();
    if process::alive(pid) {
        // Without a recorded start time (it could not be read), being alive has to do.
        return started.is_empty() || process::start_time(pid).as_deref() == Some(started);
    }
    // The first process is gone, but on Unix what it started runs on in its process group.
    cfg!(unix) && process::group_alive(pid)
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
    run_in_scratch(
        claude,
        scratch,
        timeout,
        &["-p", "/usage", "--output-format", "json"],
        [OUT_FILE, ERR_FILE, DEBUG_FILE],
    )
}

/// A warm-up's prompt in the scratch profile, the same way: one turn of the smallest model, no
/// tools, no MCP servers, no session kept.
fn run_prompt(claude: &Path, scratch: &Path, timeout: Duration) -> Result<Run> {
    run_in_scratch(
        claude,
        scratch,
        timeout,
        &[
            "-p",
            WARM_PROMPT,
            "--model",
            WARM_MODEL,
            "--tools",
            "",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--output-format",
            "json",
        ],
        [WARM_OUT_FILE, WARM_ERR_FILE, WARM_DEBUG_FILE],
    )
}

/// Claude Code with `args` in the scratch profile, its output, errors and debug log in `files`.
fn run_in_scratch(
    claude: &Path,
    scratch: &Path,
    timeout: Duration,
    args: &[&str],
    files: [&str; 3],
) -> Result<Run> {
    let [out_file, err_file, debug_file] = files;
    let out = std::fs::File::create(scratch.join(out_file))?;
    let err = std::fs::File::create(scratch.join(err_file))?;
    let mut cmd = Command::new(claude);
    cmd.args(args)
        .arg("--debug-file")
        .arg(scratch.join(debug_file))
        .current_dir(scratch)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if SCRUBBED_PREFIXES.iter().any(|p| name.starts_with(p)) && !KEPT_ENV.contains(&&*name) {
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
    // From here on no error returns early: Claude Code may be renewing the login, and an error
    // that stops it counts as an interruption (`timed_out`), which holds the account back.
    let started = process::start_time(tree.id()).unwrap_or_default();
    let _ = std::fs::write(scratch.join(PID_FILE), format!("{} {started}", tree.id()));
    let deadline = Instant::now() + timeout;
    let mut run = Run::default();
    loop {
        match tree.try_wait() {
            Ok(Some(status)) => {
                run.exit_code = status.code();
                break;
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                tree.kill();
                run.timed_out = true;
                break;
            }
        }
    }
    // Whatever it started and left running goes with it.
    drop(tree);
    let _ = std::fs::remove_file(scratch.join(PID_FILE));
    run.stdout = std::fs::read_to_string(scratch.join(out_file)).unwrap_or_default();
    run.stderr = std::fs::read_to_string(scratch.join(err_file)).unwrap_or_default();
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
        return "Claude Code didn't finish in time".into();
    }
    let logged = debug_log
        .lines()
        .rev()
        .find(|l| l.contains("Failed to load usage data"))
        .and_then(|l| l.split("Failed to load usage data: ").nth(1));
    if let Some(why) = logged {
        return format!("Claude Code couldn't read the usage: {}", why.trim());
    }
    let last = run.stderr.lines().rev().find(|l| !l.trim().is_empty());
    match (run.exit_code, last) {
        (Some(0), _) => "Claude Code didn't report any usage".into(),
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

/// The outcome in a few words for people: "5-hour 100%", "too soon, Anthropic asked to wait 24m".
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
            "too soon, Anthropic asked to wait {}",
            time::span((until - time::now_ms()) / 1000)
        ),
        CheckOutcome::LoginRejected => "login rejected. Sign in again.".into(),
        CheckOutcome::Failed { why } => format!("failed: {why}"),
    }
}

/// A line for people about a warm-up.
pub fn describe_warm_up(outcome: &WarmOutcome) -> String {
    match outcome {
        WarmOutcome::Sent { cost_usd, check } => format!(
            "sent its request (${cost_usd:.4} at API prices), so its 5-hour window is running. Now: {}",
            describe(check)
        ),
        WarmOutcome::Skipped { why } => format!("skipped: {why}"),
        WarmOutcome::Failed { why, check } => format!(
            "the request didn't go through: {why}. Now: {}",
            describe(check)
        ),
        WarmOutcome::Unexpected { why, check } => format!(
            "the request went through, but {why}, so cc-rotate stopped keeping windows running. Now: {}",
            describe(check)
        ),
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

    /// A check reads the account's usage with the account's own login, so the reset times it
    /// reads are the account's own. Without them, once the account is in use, a session still
    /// showing the account it used before (idle since that swap) could not be told apart: its
    /// reading shares a reset time with that account, and the active account's own were not
    /// known, so it counted as the active account's and its usage jumped about.
    #[test]
    fn a_check_vouches_for_the_reset_times_it_read() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let b = account(&e, "b", "ub", AccountState::Active);
        let run = fake(Fake {
            stamp: Some("ua".into()),
            pct: 5.0,
            ..Default::default()
        });
        check_with(&e.store, &e.paths, a, Trigger::Background, &run).unwrap();
        let now_s = time::now_s();
        // The fake's window resets on 2099-01-01.
        let own = swap::known_resets(&e.store, a, now_s).unwrap();
        assert!(
            own.iter().any(|&r| swap::same_reset(r, 4_070_908_800)),
            "{own:?}"
        );

        // b was in use until now, its window resetting in an hour; then a session idle since
        // shows b's numbers while a is in use.
        let window = |pct: f64, resets: i64| SampleWindow {
            name: "five_hour".into(),
            pct,
            resets_at: Some(resets),
        };
        let reading = |account: i64, pct: f64, resets: i64| Sample {
            ts: time::now_ms(),
            account_id: account,
            profile: "rotate".into(),
            session_id: Some("idle".into()),
            model: None,
            windows: vec![window(pct, resets)],
        };
        e.store
            .record_sample(&reading(b, 96.0, now_s + 3600))
            .unwrap();
        let stale = reading(a, 40.0, now_s + 3600);
        assert_eq!(
            swap::attribute(&e.store, &stale, a, now_s).unwrap(),
            swap::Attribution::Elsewhere
        );
        // A genuine reading of a whose window resets when b's latest does too stays a's.
        let mut genuine = reading(a, 7.0, 4_070_908_800);
        e.store
            .record_sample(&reading(b, 96.0, 4_070_908_800))
            .unwrap();
        genuine.ts += 1;
        assert_eq!(
            swap::attribute(&e.store, &genuine, a, now_s).unwrap(),
            swap::Attribution::Own
        );
    }

    /// An account with no identity to compare the report with is read, but its reset times are
    /// not taken as its own.
    #[test]
    fn a_check_of_an_account_without_an_identity_vouches_for_nothing() {
        let e = env();
        let ident = AccountIdentity {
            email: Some("n@x.test".into()),
            ..Default::default()
        };
        let n = e
            .store
            .insert_account("n", &ident, AccountState::Parked)
            .unwrap();
        e.store.save_credential(n, &oauth("n"), "login").unwrap();
        let run = fake(Fake {
            stamp: Some("un".into()),
            pct: 5.0,
            ..Default::default()
        });
        let out = check_with(&e.store, &e.paths, n, Trigger::Background, &run).unwrap();
        assert!(matches!(out, CheckOutcome::Read { .. }), "{out:?}");
        assert!(
            swap::known_resets(&e.store, n, time::now_s())
                .unwrap()
                .is_empty()
        );
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
            scheduler::pick(&pool, policy, None, time::now_s()).is_none(),
            "a held-back account is not picked"
        );
        assert_eq!(
            next_due(&e.store, &e.paths, time::now_ms() + HELD_BACK_RETRY_MS + 1).unwrap(),
            Some(a)
        );
        // Still unreadable: recovery keeps it and the account stays out.
        assert_eq!(
            recover(&e.store, &e.paths, a).unwrap(),
            Recovery::Busy("the login an earlier usage check left can't be read yet".into())
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
        // (The user asking now; the background would wait for the retry after a failure.)
        let out = check_with(&e.store, &e.paths, a, Trigger::Manual, &settle).unwrap();
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

    /// A check's leftover: its start fingerprint, then (optionally) the login it left.
    fn leave(
        e: &Env,
        a: i64,
        tag: &str,
        started: i64,
        start_fp: &str,
        login: Option<&str>,
    ) -> PathBuf {
        let dir = e
            .paths
            .staging_root()
            .join(format!("{SCRATCH_PREFIX}{a}-{started}-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(START_FP_FILE), start_fp).unwrap();
        if let Some(text) = login {
            std::fs::write(dir.join(".credentials.json"), text).unwrap();
        }
        dir
    }

    fn login_text(tag: &str) -> String {
        json!({ "claudeAiOauth": oauth(tag) }).to_string()
    }

    #[test]
    fn recovery_stores_a_renewal_the_check_left() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let start = e.store.credential_meta(a).unwrap().unwrap().refresh_fp;
        let dir = leave(&e, a, "x", 1, &start, Some(&login_text("a-renewed")));
        assert_eq!(recover(&e.store, &e.paths, a).unwrap(), Recovery::Clean);
        assert_eq!(
            e.store.credential(a).unwrap().unwrap().oauth["refreshToken"],
            "refresh-a-renewed"
        );
        assert!(!dir.exists() && !state(&e.store, a).unwrap().held_back);
    }

    #[test]
    fn a_leftover_that_settles_nothing_holds_the_account_back() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let start = e.store.credential_meta(a).unwrap().unwrap().refresh_fp;
        // The login unchanged: the check never committed, so a renewal may have been lost.
        let unchanged = leave(&e, a, "x", 1, &start, Some(&login_text("a")));
        assert_eq!(recover(&e.store, &e.paths, a).unwrap(), Recovery::Clean);
        assert!(!unchanged.exists());
        assert!(state(&e.store, a).unwrap().held_back);

        // No login left at all.
        forget(&e.store, a).unwrap();
        leave(&e, a, "y", 1, &start, None);
        recover(&e.store, &e.paths, a).unwrap();
        assert!(state(&e.store, a).unwrap().held_back);
    }

    #[test]
    fn a_leftover_from_before_a_relogin_is_discarded_unread() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let dir = leave(&e, a, "x", 1, "an-older-login", Some("{\"claudeAi"));
        assert_eq!(recover(&e.store, &e.paths, a).unwrap(), Recovery::Clean);
        assert!(!dir.exists());
        assert!(!state(&e.store, a).unwrap().held_back);
        // Without a start fingerprint, Claude Code never got the login either.
        let no_start = e
            .paths
            .staging_root()
            .join(format!("{SCRATCH_PREFIX}{a}-1-z"));
        std::fs::create_dir_all(&no_start).unwrap();
        recover(&e.store, &e.paths, a).unwrap();
        assert!(!no_start.exists() && !state(&e.store, a).unwrap().held_back);
    }

    #[test]
    fn an_unreadable_leftover_is_kept_a_while_then_given_up() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let start = e.store.credential_meta(a).unwrap().unwrap().refresh_fp;
        let recent = leave(&e, a, "x", time::now_ms(), &start, Some("{\"claudeAi"));
        assert!(matches!(
            recover(&e.store, &e.paths, a).unwrap(),
            Recovery::Busy(_)
        ));
        assert!(recent.exists() && state(&e.store, a).unwrap().held_back);
        std::fs::remove_dir_all(&recent).unwrap();

        let old = leave(
            &e,
            a,
            "y",
            time::now_ms() - LEFTOVER_KEPT_MS - 1,
            &start,
            Some("{\"claudeAi"),
        );
        assert_eq!(recover(&e.store, &e.paths, a).unwrap(), Recovery::Clean);
        assert!(!old.exists());
        assert!(
            state(&e.store, a).unwrap().held_back,
            "the next check settles it by renewing"
        );
    }

    #[test]
    fn a_still_running_check_is_left_alone_but_a_reused_process_id_is_not() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let start = e.store.credential_meta(a).unwrap().unwrap().refresh_fp;
        let me = std::process::id();
        let live = leave(&e, a, "x", 1, &start, Some(&login_text("a")));
        let started = process::start_time(me).unwrap();
        std::fs::write(live.join(PID_FILE), format!("{me} {started}")).unwrap();
        assert!(matches!(
            recover(&e.store, &e.paths, a).unwrap(),
            Recovery::Busy(_)
        ));
        assert!(live.exists());

        // The same id, but another process (it started at another time).
        std::fs::write(live.join(PID_FILE), format!("{me} 12345")).unwrap();
        assert_eq!(recover(&e.store, &e.paths, a).unwrap(), Recovery::Clean);
        assert!(!live.exists());
    }

    /// A `/login` in the profile changed the account while its check ran: the check's verdict on
    /// the old login is not stored over the fresh one.
    #[test]
    fn a_check_does_not_overwrite_a_login_that_changed_meanwhile() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let run = |dir: &Path| {
            e.store
                .save_credential(a, &oauth("a-fresh"), "sync")
                .unwrap();
            fake(Fake {
                stamp: Some("ua".into()),
                renew: Some(Some(oauth("a-renewed-old"))),
                ..Default::default()
            })(dir)
        };
        let out = check_with(&e.store, &e.paths, a, Trigger::Manual, &run).unwrap();
        assert!(matches!(out, CheckOutcome::Read { .. }), "{out:?}");
        assert_eq!(
            e.store.credential(a).unwrap().unwrap().oauth["refreshToken"],
            "refresh-a-fresh"
        );
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

    /// Claude Code's result for the warm-up's one short Haiku turn.
    const HAIKU_RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"num_turns":1,"total_cost_usd":0.0009,"modelUsage":{"claude-haiku-5-5":{"costUSD":0.0009}},"result":"OK"}"#;

    /// A fake warm-up prompt run: prints `stdout`, and renews the login when `renew` is given.
    fn prompt_fake(
        stdout: &str,
        renew: Option<Value>,
        timed_out: bool,
    ) -> impl Fn(&Path) -> Result<Run> {
        let stdout = stdout.to_string();
        move |dir: &Path| {
            if let Some(renew) = &renew {
                credfile::write_oauth(&dir.join(".credentials.json"), renew).unwrap();
            }
            Ok(Run {
                exit_code: Some(0),
                timed_out,
                stdout: stdout.clone(),
                ..Default::default()
            })
        }
    }

    fn reading(account_id: i64, ts: i64, windows: &[(&str, f64, i64)]) -> Sample {
        Sample {
            ts,
            account_id,
            profile: SOURCE.into(),
            session_id: None,
            model: None,
            windows: windows
                .iter()
                .map(|&(name, pct, resets_at)| SampleWindow {
                    name: name.into(),
                    pct,
                    resets_at: Some(resets_at),
                })
                .collect(),
        }
    }

    #[test]
    fn a_warm_up_sends_one_short_prompt_then_reads_the_usage() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        e.store.set_keep_running(a, true).unwrap();
        // The prompt run renews the login; the reading after it runs with the renewal.
        let prompt = prompt_fake(HAIKU_RESULT, Some(oauth("a-renewed")), false);
        let usage = fake(Fake {
            stamp: Some("ua".into()),
            pct: 1.0,
            ..Default::default()
        });
        let out = warm_up_with(&e.store, &e.paths, a, Trigger::WarmUp, &prompt, &usage).unwrap();
        let WarmOutcome::Sent { cost_usd, check } = &out else {
            panic!("{out:?}")
        };
        assert_eq!(*cost_usd, 0.0009);
        assert!(matches!(check, CheckOutcome::Read { .. }), "{check:?}");
        assert_eq!(
            e.store.credential(a).unwrap().unwrap().oauth["refreshToken"],
            "refresh-a-renewed"
        );
        let st = state(&e.store, a).unwrap();
        assert!(st.warmed_at.is_some() && st.warm_attempt_at.is_some());
        assert_eq!(st.warm_problem, None);
        let event = e.store.last_event_of(kind::WARM_UP).unwrap().unwrap();
        assert_eq!(event.account_from, Some(a));
        assert_eq!(event.detail.unwrap()["cost_usd"], 0.0009);
        assert_eq!(scratch_dirs_left(&e), 0);
        assert_eq!(
            next_warm_up(&e.store, &e.paths, time::now_ms()).unwrap(),
            None,
            "its window runs now"
        );
        assert!(describe_warm_up(&out).starts_with("sent its request"));
    }

    #[test]
    fn a_warm_up_that_did_more_than_asked_stops_warm_ups() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        e.store.set_keep_running(a, true).unwrap();
        let opus = HAIKU_RESULT.replace("claude-haiku-5-5", "claude-opus-5-5");
        let usage = fake(Fake {
            stamp: Some("ua".into()),
            ..Default::default()
        });
        let out = warm_up_with(
            &e.store,
            &e.paths,
            a,
            Trigger::WarmUp,
            &prompt_fake(&opus, None, false),
            &usage,
        )
        .unwrap();
        let WarmOutcome::Unexpected { why, .. } = &out else {
            panic!("{out:?}")
        };
        assert!(why.contains("claude-opus-5-5"), "{why}");
        assert!(describe_warm_up(&out).contains("went through"));
        let reason = warm_ups_blocked(&e.store).unwrap().expect("stopped");
        assert!(e.store.last_event_of(kind::ERROR).unwrap().is_some());
        assert!(e.store.last_event_of(kind::WARM_UP).unwrap().is_none());
        assert_eq!(
            warm_up(&e.store, &e.paths, a, Trigger::WarmUp).unwrap(),
            WarmOutcome::Skipped {
                why: format!("keeping windows running is stopped because {reason}")
            }
        );
        // Turning it on again for an account lets warm-ups run again.
        e.store.set_keep_running(a, true).unwrap();
        assert_eq!(warm_ups_blocked(&e.store).unwrap(), None);

        for (stdout, expect) in [
            (
                HAIKU_RESULT.replace(r#""num_turns":1"#, r#""num_turns":3"#),
                "3 turns",
            ),
            (
                HAIKU_RESULT.replace(r#""total_cost_usd":0.0009"#, r#""total_cost_usd":0.5"#),
                "$0.5000",
            ),
        ] {
            let run = Ok(Run {
                stdout,
                ..Default::default()
            });
            let Prompted::Unexpected { why } = judge_warm_up(&run) else {
                panic!("{expect}")
            };
            assert!(why.contains(expect), "{why}");
        }
    }

    #[test]
    fn a_warm_up_that_did_not_go_through_is_tried_again_later() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        e.store.set_keep_running(a, true).unwrap();
        let limit = r#"{"type":"result","subtype":"success","is_error":true,"num_turns":0,"total_cost_usd":0,"modelUsage":{},"result":"Claude AI usage limit reached"}"#;
        let usage = fake(Fake {
            stamp: Some("ua".into()),
            ..Default::default()
        });
        let out = warm_up_with(
            &e.store,
            &e.paths,
            a,
            Trigger::WarmUp,
            &prompt_fake(limit, None, false),
            &usage,
        )
        .unwrap();
        assert!(
            matches!(&out, WarmOutcome::Failed { why, .. } if why == "Claude AI usage limit reached"),
            "{out:?}"
        );
        let st = state(&e.store, a).unwrap();
        assert_eq!(st.warmed_at, None);
        assert_eq!(
            st.warm_problem.as_deref(),
            Some("Claude AI usage limit reached")
        );
        assert_eq!(warm_ups_blocked(&e.store).unwrap(), None);
        assert!(e.store.last_event_of(kind::WARM_UP).unwrap().is_none());

        // An interrupted prompt run may have renewed the login: the account is held back.
        let b = account(&e, "b", "ub", AccountState::Parked);
        e.store.set_keep_running(b, true).unwrap();
        warm_up_with(
            &e.store,
            &e.paths,
            b,
            Trigger::WarmUp,
            &prompt_fake("", None, true),
            &fake(Fake {
                stamp: Some("ub".into()),
                ..Default::default()
            }),
        )
        .unwrap();
        assert!(state(&e.store, b).unwrap().held_back);

        // Only accounts the user opted in get one from the background.
        let c = account(&e, "c", "uc", AccountState::Parked);
        let out = warm_up_with(
            &e.store,
            &e.paths,
            c,
            Trigger::WarmUp,
            &prompt_fake(HAIKU_RESULT, None, false),
            &usage,
        )
        .unwrap();
        assert!(matches!(out, WarmOutcome::Skipped { .. }), "{out:?}");
    }

    #[test]
    fn warm_ups_go_to_opted_in_accounts_whose_window_ended() {
        let e = env();
        let now = time::now_ms();
        let now_s = now / 1000;
        let a = account(&e, "a", "ua", AccountState::Parked);
        let b = account(&e, "b", "ub", AccountState::Parked);
        let c = account(&e, "c", "uc", AccountState::Active);
        let due = |at: i64| next_warm_up(&e.store, &e.paths, at).unwrap();
        assert_eq!(due(now), None, "no account opted in");

        e.store.set_keep_running(a, true).unwrap();
        e.store.set_keep_running(c, true).unwrap();
        assert_eq!(due(now), Some(a), "never the account in use");
        assert_eq!(
            next_job(&e.store, &e.paths, now).unwrap(),
            Some(Due::WarmUp(a)),
            "before a routine check"
        );

        let record = |windows: &[(&str, f64, i64)]| {
            e.store
                .record_sample(&reading(a, now - 1000, windows))
                .unwrap()
        };
        record(&[("five_hour", 10.0, now_s + 3600)]);
        assert_eq!(due(now), None, "its window is running");
        assert_eq!(due(now + 2 * 3_600_000), Some(a), "and then it ended");

        record(&[
            ("five_hour", 0.0, now_s - 60),
            ("seven_day", 99.0, now_s + 86_400),
        ]);
        assert_eq!(due(now), None, "its weekly limit keeps it out of use");

        record(&[
            ("five_hour", 0.0, now_s - 60),
            ("seven_day", 10.0, now_s + 86_400),
        ]);
        assert_eq!(due(now), Some(a));
        // A window a warm-up started runs even without a reading after it.
        save_state(
            &e.store,
            a,
            &CheckState {
                warmed_at: Some(now - 60_000),
                warm_attempt_at: Some(now - 60_000),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(due(now), None);
        assert_eq!(due(window_end_ms(now - 60_000) + 1), Some(a));
        // A try that did not go through waits a while.
        save_state(
            &e.store,
            a,
            &CheckState {
                warm_attempt_at: Some(now - 60_000),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(due(now), None);
        assert_eq!(due(now - 60_000 + WARM_RETRY_MS), Some(a));

        e.store.set_meta(META_WARM_BLOCKED, "a test").unwrap();
        assert_eq!(due(now + WARM_RETRY_MS), None, "stopped");
        e.store.delete_meta(META_WARM_BLOCKED).unwrap();
        e.store.set_setting("usage_checks", Some("false")).unwrap();
        assert_eq!(due(now + WARM_RETRY_MS), None, "they need usage checks");
        e.store.set_setting("usage_checks", Some("true")).unwrap();
        assert_eq!(due(now + WARM_RETRY_MS), Some(a));

        // A held-back account's check comes first: it is out of use until settled.
        save_state(
            &e.store,
            b,
            &CheckState {
                held_back: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            next_job(&e.store, &e.paths, now + WARM_RETRY_MS).unwrap(),
            Some(Due::Check(b))
        );
    }

    #[test]
    fn a_window_ends_five_hours_after_its_ten_minute_mark() {
        let start = 1_800_000_000_000 - 1_800_000_000_000 % WINDOW_START_STEP_MS;
        assert_eq!(window_end_ms(start), start + FIVE_HOURS_MS);
        assert_eq!(window_end_ms(start + 9 * 60_000), start + FIVE_HOURS_MS);
        assert_eq!(
            window_end_ms(start + 10 * 60_000),
            start + 10 * 60_000 + FIVE_HOURS_MS
        );
    }

    /// A warm-up that cannot run (here: its login is the one the cc-rotate profile uses) counts
    /// as a try, so the background goes on to other work instead of retrying it every minute.
    #[test]
    fn a_warm_up_that_cannot_run_lets_other_work_go_first() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let b = account(&e, "b", "ub", AccountState::Parked);
        e.store.set_keep_running(a, true).unwrap();
        let login = e.store.credential(a).unwrap().unwrap().oauth;
        credfile::write_oauth(&e.paths.profile_dir.join(".credentials.json"), &login).unwrap();
        let now = time::now_ms();
        assert_eq!(
            next_job(&e.store, &e.paths, now).unwrap(),
            Some(Due::WarmUp(a))
        );
        let never = |_: &Path| -> Result<Run> { panic!("nothing may run") };
        let out = warm_up_with(&e.store, &e.paths, a, Trigger::WarmUp, &never, &never).unwrap();
        assert!(matches!(out, WarmOutcome::Skipped { .. }), "{out:?}");
        assert_eq!(
            next_job(&e.store, &e.paths, time::now_ms()).unwrap(),
            Some(Due::Check(b))
        );
    }

    /// A warm-up that did more than asked stops warm-ups even when the login it left cannot be
    /// read (the account is held back for that as usual).
    #[test]
    fn an_unexpected_warm_up_stops_warm_ups_even_with_an_unreadable_login() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        e.store.set_keep_running(a, true).unwrap();
        let opus = HAIKU_RESULT.replace("claude-haiku-5-5", "claude-opus-5-5");
        let prompt = move |dir: &Path| {
            std::fs::write(dir.join(".credentials.json"), "{\"claudeAi").unwrap();
            Ok(Run {
                exit_code: Some(0),
                stdout: opus.clone(),
                ..Default::default()
            })
        };
        let out = warm_up_with(
            &e.store,
            &e.paths,
            a,
            Trigger::WarmUp,
            &prompt,
            &fake(Fake::default()),
        )
        .unwrap();
        assert!(matches!(out, WarmOutcome::Unexpected { .. }), "{out:?}");
        assert!(warm_ups_blocked(&e.store).unwrap().is_some());
        let st = state(&e.store, a).unwrap();
        assert!(st.held_back && st.warmed_at.is_some());
    }

    /// While the usage endpoint asked to wait, a warm-up sends its prompt but leaves out the
    /// reading after it; the window it started still counts.
    #[test]
    fn a_warm_up_leaves_out_the_reading_while_the_endpoint_asks_to_wait() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        e.store.set_keep_running(a, true).unwrap();
        let until = time::now_ms() + 600_000;
        save_state(
            &e.store,
            a,
            &CheckState {
                not_before: Some(until),
                ..Default::default()
            },
        )
        .unwrap();
        let never = |_: &Path| -> Result<Run> { panic!("no /usage while asked to wait") };
        let out = warm_up_with(
            &e.store,
            &e.paths,
            a,
            Trigger::WarmUp,
            &prompt_fake(HAIKU_RESULT, None, false),
            &never,
        )
        .unwrap();
        let WarmOutcome::Sent { cost_usd, check } = &out else {
            panic!("{out:?}")
        };
        assert_eq!(*cost_usd, 0.0009);
        assert!(
            matches!(check, CheckOutcome::Skipped { why } if why.starts_with("the reading waits until")),
            "{check:?}"
        );
        assert_eq!(state(&e.store, a).unwrap().not_before, Some(until));
        assert_eq!(
            next_warm_up(&e.store, &e.paths, time::now_ms()).unwrap(),
            None
        );
    }

    /// A reading newer than a warm-up says when the window really ends: the prompt may have
    /// landed in a window that was already running.
    #[test]
    fn a_reading_after_a_warm_up_says_when_its_window_ends() {
        let e = env();
        let now = time::now_ms();
        let a = account(&e, "a", "ua", AccountState::Parked);
        e.store.set_keep_running(a, true).unwrap();
        save_state(
            &e.store,
            a,
            &CheckState {
                warmed_at: Some(now - 60_000),
                warm_attempt_at: Some(now - 60_000),
                ..Default::default()
            },
        )
        .unwrap();
        e.store
            .record_sample(&reading(a, now, &[("five_hour", 30.0, now / 1000 + 3600)]))
            .unwrap();
        let due = |at: i64| next_warm_up(&e.store, &e.paths, at).unwrap();
        assert_eq!(due(now), None);
        assert_eq!(due(now + 3_600_000 + 60_000), Some(a));
    }

    /// Without `claude` to run, a warm-up fails like any other that did not go through, and is
    /// retried later instead of every minute.
    #[test]
    fn a_warm_up_without_claude_is_retried_later() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        let b = account(&e, "b", "ub", AccountState::Parked);
        e.store.set_keep_running(a, true).unwrap();
        let nowhere = e.paths.data_dir.join("no-claude-here");
        e.store
            .set_setting("claude_path", Some(&nowhere.display().to_string()))
            .unwrap();
        let out = warm_up(&e.store, &e.paths, a, Trigger::WarmUp).unwrap();
        assert!(matches!(out, WarmOutcome::Failed { .. }), "{out:?}");
        let st = state(&e.store, a).unwrap();
        assert!(st.warm_attempt_at.is_some() && st.warm_problem.is_some());
        assert!(!st.held_back, "nothing ran, so the login is not in doubt");
        assert_eq!(scratch_dirs_left(&e), 0);
        assert_eq!(
            next_job(&e.store, &e.paths, time::now_ms()).unwrap(),
            Some(Due::Check(b))
        );
    }

    /// A reading after a warm-up that shows no window (it may lag behind the request) does not
    /// undo the window the warm-up started.
    #[test]
    fn a_reading_without_the_window_does_not_undo_a_warm_up() {
        let e = env();
        let now = time::now_ms();
        let a = account(&e, "a", "ua", AccountState::Parked);
        e.store.set_keep_running(a, true).unwrap();
        save_state(
            &e.store,
            a,
            &CheckState {
                warmed_at: Some(now - 60_000),
                warm_attempt_at: Some(now - 60_000),
                ..Default::default()
            },
        )
        .unwrap();
        // The window before the warm-up, carried in the reading after it.
        e.store
            .record_sample(&reading(a, now, &[("five_hour", 0.0, now / 1000 - 600)]))
            .unwrap();
        let due = |at: i64| next_warm_up(&e.store, &e.paths, at).unwrap();
        assert_eq!(due(now + WARM_RETRY_MS), None);
        assert_eq!(due(window_end_ms(now - 60_000) + 1), Some(a));
    }

    /// Should the prompt run ask the usage endpoint and be refused, the wait is kept.
    #[test]
    fn a_refusal_in_the_prompt_run_is_kept() {
        let e = env();
        let a = account(&e, "a", "ua", AccountState::Parked);
        e.store.set_keep_running(a, true).unwrap();
        let prompt = |dir: &Path| {
            std::fs::write(
                dir.join(WARM_DEBUG_FILE),
                "[ERROR] fetchUtilization failed: 429\\n[DEBUG] 429 remembered for this bearer; not asking again for 1800s\\n",
            )
            .unwrap();
            Ok(Run {
                exit_code: Some(0),
                stdout: HAIKU_RESULT.into(),
                ..Default::default()
            })
        };
        let usage = fake(Fake {
            stamp: Some("ua".into()),
            ..Default::default()
        });
        let before = time::now_ms();
        warm_up_with(&e.store, &e.paths, a, Trigger::WarmUp, &prompt, &usage).unwrap();
        let not_before = state(&e.store, a).unwrap().not_before.expect("a wait");
        assert!(not_before >= before + 1_800_000, "{}", not_before - before);
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
            "Claude Code couldn't read the usage: Request failed with status code 500"
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
        assert!(ran_as_prompt(r#"{"num_turns":0,"total_cost_usd":0.04}"#).is_some());
        assert_eq!(ran_as_prompt(""), None, "no result to judge");
        assert_eq!(
            ran_as_prompt(
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":0}"#
            ),
            None,
            "an error is not a prompt"
        );
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
