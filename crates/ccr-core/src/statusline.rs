//! The statusline tick (spec section 8): record usage, write renewed logins back to the store,
//! detect dead or foreign logins, and decide whether a swap is needed.
//!
//! This runs after every reply and every 30 seconds in every session, so it must stay fast and
//! must never fail in a way that breaks the user's status line; callers log errors and carry on.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use crate::paths::Paths;
use crate::profile_account;
use crate::relay::{self, RelayConfig};
use crate::scheduler::{self, Reason, Timing};
use crate::store::{
    Account, AccountState, NAMED_PROCESS_PREFIX, Sample, SampleWindow, Store, kind,
};
use crate::swap::{
    self, META_CRED_SEEN, META_LAST_SWAP_AT, META_PROFILE_FOREIGN, META_PROFILE_MODEL,
    STALE_SAMPLE_GUARD_MS, SwapRequest,
};
use crate::{credfile, herdr, time};

pub const META_LAST_TICK_AT: &str = "statusline_last_tick_at";
/// When a tick last asked for a usage check of the account in use (`confirm_due`).
const META_CONFIRM_REQUESTED_AT: &str = "confirm_requested_at";
const CONFIRM_QUIET_MS: i64 = 10 * 60_000;
pub const META_LAST_TICK_HAD_USAGE: &str = "statusline_last_tick_had_usage";
const META_CONFIG_MTIME_SEEN: &str = "profile_config_mtime_seen";
const META_PROFILE_UUID_SEEN: &str = "profile_account_uuid_seen";
/// When a swap was last asked for, which throttles asking again (`scheduler::Timing`).
pub const META_SWAP_REQUESTED_AT: &str = "swap_requested_at";
/// The credentials change marker a dead login was last reported for, so it is logged once.
const META_DEAD_LOGIN_SEEN: &str = "dead_login_seen";

/// The parts of Claude Code's status line input cc-rotate reads.
#[derive(Debug, Default, Deserialize)]
pub struct StatusInput {
    pub session_id: Option<String>,
    /// The Claude Code version the session runs (`health::note_claude_version`).
    pub version: Option<String>,
    /// What the session's use would have cost at API prices so far, as Claude Code estimates it.
    pub cost: Option<CostInfo>,
    pub model: Option<ModelInfo>,
    /// Where the session was started (`workspace.project_dir`), falling back to the working
    /// directory when that field is absent (`launch_dir`).
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub workspace: Option<WorkspaceInfo>,
    /// Every usage window Claude Code reports, by name (`five_hour`, `seven_day`, ...). Kept
    /// generic so a window Claude Code adds later is recorded without a code change; a window
    /// is only present while the API reports it and its reset has not passed.
    #[serde(default, deserialize_with = "lenient_windows")]
    pub rate_limits: Option<BTreeMap<String, Window>>,
    /// The Claude Code process the status line runs for, from its `CLAUDE_PID` environment
    /// variable (`claude_pid_from_env`), not the JSON.
    #[serde(skip)]
    pub claude_pid: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
pub struct WorkspaceInfo {
    #[serde(default)]
    pub project_dir: Option<String>,
    #[serde(default)]
    pub current_dir: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct CostInfo {
    pub total_cost_usd: Option<f64>,
    /// How long the session's process has run, restored with its cost on `--resume` and restarted
    /// with it by `/login`: `now -
    /// total_duration_ms` is about when the process started, give or take how late the status
    /// line started, and it falls minutes behind the clock over days (F25).
    pub total_duration_ms: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ModelInfo {
    pub id: Option<String>,
}

/// A map of windows, skipping entries that are not window objects instead of failing the
/// whole input.
fn lenient_windows<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<BTreeMap<String, Window>>, D::Error> {
    let Some(raw) = Option::<BTreeMap<String, Value>>::deserialize(d)? else {
        return Ok(None);
    };
    Ok(Some(
        raw.into_iter()
            .filter_map(|(k, v)| serde_json::from_value::<Window>(v).ok().map(|w| (k, w)))
            .collect(),
    ))
}

#[derive(Debug, Default, Deserialize)]
pub struct Window {
    pub used_percentage: Option<f64>,
    #[serde(default, deserialize_with = "epoch_seconds")]
    pub resets_at: Option<i64>,
}

/// Accept epoch seconds as a number, a numeric string, or an ISO 8601 timestamp.
fn epoch_seconds<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Num(f64),
        Text(String),
    }
    Ok(match Option::<Raw>::deserialize(d)? {
        Some(Raw::Num(n)) => time::epoch_s(n),
        Some(Raw::Text(s)) => match s.parse::<f64>() {
            Ok(n) => time::epoch_s(n),
            Err(_) => time::parse_rfc3339(&s),
        },
        None => None,
    })
}

impl StatusInput {
    pub fn parse(text: &str) -> Self {
        serde_json::from_str(text).unwrap_or_default()
    }

    pub fn model_id(&self) -> Option<&str> {
        self.model
            .as_ref()
            .and_then(|m| m.id.as_deref())
            .filter(|m| !m.is_empty())
    }

    /// The folder Claude Code was launched in, or the working directory when the launch folder
    /// was not sent. Blank strings are absent.
    pub fn launch_dir(&self) -> Option<&str> {
        fn nonempty(s: &Option<String>) -> Option<&str> {
            s.as_deref().filter(|p| !p.trim().is_empty())
        }
        self.workspace
            .as_ref()
            .and_then(|w| nonempty(&w.project_dir))
            .or_else(|| nonempty(&self.cwd))
            .or_else(|| {
                self.workspace
                    .as_ref()
                    .and_then(|w| nonempty(&w.current_dir))
            })
    }

    fn sample_for(&self, account_id: i64, profile: &str, now_ms: i64) -> Option<Sample> {
        let windows: Vec<SampleWindow> = self
            .rate_limits
            .as_ref()?
            .iter()
            .filter_map(|(name, w)| {
                Some(SampleWindow {
                    name: name.clone(),
                    pct: w.used_percentage?,
                    resets_at: w.resets_at,
                })
            })
            .collect();
        if windows.is_empty() {
            return None;
        }
        Some(Sample {
            ts: now_ms,
            account_id,
            profile: profile.to_string(),
            session_id: self.session_id.clone(),
            model: self.model_id().map(str::to_string),
            windows,
        })
    }
}

#[derive(Debug, Default)]
pub struct TickResult {
    pub active: Option<Account>,
    pub swap: Option<SwapRequest>,
    pub pool_exhausted: bool,
    /// Start a usage check of the account in use (`usage_check::confirm_active`) in the
    /// background: readings at its limit keep coming that look like another account's, while
    /// none of its own arrived for a while.
    pub confirm: bool,
    /// Start `cc-rotate-ctl settle-login` in the background: the profile's login has to be
    /// identified, after a swap stopped half way or because it changed without cc-rotate
    /// writing it (`swap::settle_profile`).
    pub settle_login: bool,
}

/// One statusline tick in the rotation profile.
///
/// `env` looks up a variable of the session's environment, which the status line inherits.
pub fn tick(
    store: &Store,
    paths: &Paths,
    input: &StatusInput,
    now_ms: i64,
    env: impl Fn(&str) -> Option<String>,
) -> Result<TickResult> {
    let mut result = TickResult::default();
    let mut dead_login = false;
    let mut unidentified = false;

    // Recorded for `doctor`: proves the statusline runs, and whether Claude Code sent usage.
    store.set_meta(META_LAST_TICK_AT, now_ms)?;
    store.set_meta(
        META_LAST_TICK_HAD_USAGE,
        if input.sample_for(0, "", now_ms).is_some() {
            "1"
        } else {
            "0"
        },
    )?;

    // In relay mode the relay records usage and asks for threshold swaps (spec 7). A session
    // that bypasses the relay runs on the home account directly, so its cost is the home
    // account's.
    let relay_mode = relay::home(store)?.is_some();
    let bypasses = relay_mode && note_relay_bypass(store, paths, input, &env, now_ms)?;
    if !relay_mode {
        note_stale_relay_session(store, input, &env, now_ms)?;
    }
    let cost_for = |active_id: i64| -> Result<i64> {
        Ok(match bypasses {
            true => relay::home(store)?.unwrap_or(active_id),
            false => active_id,
        })
    };

    let process = process_key(store, input, now_ms)?;
    if !sync_profile_login(store, paths, &mut dead_login, &mut unidentified)? {
        // Nothing can be trusted about the profile's login until the swap is settled. The cost
        // still counts, for the account cc-rotate last put in the profile.
        result.settle_login = settle_due(store, now_ms)?;
        result.active = store.active_account()?;
        if let Some(active) = &result.active {
            note_cost(store, input, &process, cost_for(active.id)?, now_ms)?;
        }
        return Ok(result);
    }
    if unidentified && !crate::lock::is_held(&paths.swap_lock(), crate::lock::CLAUDE_STALE) {
        result.settle_login = settle_due(store, now_ms)?;
    }

    let Some(active) = store.active_account()? else {
        return Ok(result);
    };

    if let Some(model) = input.model_id()
        && store.meta(META_PROFILE_MODEL)?.as_deref() != Some(model)
    {
        store.set_meta(META_PROFILE_MODEL, model)?;
    }
    if let Some(version) = input.version.as_deref() {
        crate::health::note_claude_version(store, version)?;
    }
    // What the session added to its cost since its last reading was spent on the account in
    // use, unless its reading is another account's: then the request still carried that one's
    // login. In relay mode the readings are not recorded: a session's own tracker mixes
    // accounts after a swap, since its login never changes.
    let mut cost_account = cost_for(active.id)?;
    let reading = input
        .sample_for(active.id, crate::store::ROTATION_PROFILE, now_ms)
        .filter(|_| !relay_mode);
    if let Some(sample) = reading {
        if idle_since_swap(store, input, &process, &sample, now_ms)? {
            // The numbers of the account the session used before; it has not had a response
            // since.
            cost_account = reading_account(store, &sample, active.id)?.unwrap_or(active.id);
        } else if is_stale_reading(store, &sample, now_ms)? {
            // It carries the reset times of the account the last swap left.
            cost_account = left_account(store)?.unwrap_or(active.id);
        } else {
            match swap::attribute(store, &sample, active.id, now_ms / 1000)? {
                swap::Attribution::Own => {
                    store.record_sample(&sample)?;
                    swap::note_known_resets(store, active.id, &sample.windows, now_ms / 1000)?;
                    note_session_working(store, input, active.id, &sample, now_ms)?;
                }
                // Recorded, but it does not vouch for the account's reset times.
                swap::Attribution::Ambiguous => {
                    store.record_sample(&sample)?;
                    note_session_working(store, input, active.id, &sample, now_ms)?;
                }
                swap::Attribution::Elsewhere => {
                    result.confirm = confirm_due(store, &sample, active.id, now_ms)?;
                    cost_account = reading_account(store, &sample, active.id)?.unwrap_or(active.id);
                }
            }
        }
    }
    note_cost(store, input, &process, cost_account, now_ms)?;

    let (pool, policy) = swap::load_pool(store, now_ms / 1000)?;
    crate::budget::note_in_use(store, &pool)?;
    let timing = timing(store, active.id, now_ms)?;
    // Every account at a limit and the one in use paying: with `avoid_extra_usage` on, move to
    // one without extra usage first, whatever else the decision would be.
    let avoid = if dead_login {
        None
    } else {
        avoid_extra_usage(store, &pool, policy, &active, now_ms)?
    };
    if let Some(req) = avoid {
        store.set_meta(META_SWAP_REQUESTED_AT, now_ms)?;
        result.pool_exhausted = true;
        result.swap = Some(req);
    } else if let Some(reason) = scheduler::decide(&pool, policy, timing, dead_login, !relay_mode) {
        if reason != Reason::PreferredAvailable {
            result.pool_exhausted =
                scheduler::pick(&pool, policy, Some(active.id), now_ms / 1000).is_none();
        }
        store.set_meta(META_SWAP_REQUESTED_AT, now_ms)?;
        result.swap = Some(SwapRequest {
            from: Some(active.id),
            to: None,
            reason,
            wait: false,
        });
    } else if pool
        .iter()
        .any(|c| c.account.id == active.id && c.blocking().is_some())
    {
        result.pool_exhausted =
            scheduler::pick(&pool, policy, Some(active.id), now_ms / 1000).is_none();
    }

    // Paid extra usage judged by an old report: read the account in use again, in place of a
    // swap that has nowhere to go (a sync runs one thing at a time: a swap, a settling, or a
    // reading). A swap away from paying goes first.
    let futile = result.swap.as_ref().is_some_and(|s| {
        result.pool_exhausted && !matches!(s.reason, Reason::AvoidExtraUsage | Reason::DeadLogin)
    });
    if !result.confirm
        && !result.settle_login
        && (result.swap.is_none() || futile)
        && crate::extra_usage::needs_fresh_reading(store, &pool, policy, &active, now_ms)?
    {
        result.swap = None;
        result.confirm = true;
    }

    store.prune(now_ms)?;
    crate::health::scan_from_tick(store, now_ms)?;
    result.active = store.active_account()?;
    Ok(result)
}

/// In relay mode, whether the session's `ANTHROPIC_BASE_URL` is not the relay's address, so it
/// runs on the home account directly (spec 4); such a session with an id is recorded. An
/// unreadable `relay.json` does not fail the tick.
fn note_relay_bypass(
    store: &Store,
    paths: &Paths,
    input: &StatusInput,
    env: &impl Fn(&str) -> Option<String>,
    now_ms: i64,
) -> Result<bool> {
    let cfg = RelayConfig::read(&paths.relay_config()).ok().flatten();
    if !relay::bypasses(cfg.as_ref(), env(relay::ENV_BASE_URL).as_deref()) {
        return Ok(false);
    }
    if let Some(session) = input.session_id.as_deref().filter(|s| !s.is_empty()) {
        relay::note_bypass(store, session, now_ms)?;
    }
    Ok(true)
}

/// With relay mode off, record a session whose `ANTHROPIC_BASE_URL` is a relay's address: it was
/// started while relay mode was on, and gets connection errors once the relay leaves, an hour
/// after `relay off` (spec 4). `doctor` warns while any reported in the last 10 minutes.
fn note_stale_relay_session(
    store: &Store,
    input: &StatusInput,
    env: &impl Fn(&str) -> Option<String>,
    now_ms: i64,
) -> Result<()> {
    if !relay::points_at_relay(env(relay::ENV_BASE_URL).as_deref()) {
        return Ok(());
    }
    if let Some(session) = input.session_id.as_deref().filter(|s| !s.is_empty()) {
        relay::note_stale(store, session, now_ms)?;
    }
    Ok(())
}

/// A fresh reading of the account in use from a session stopped at a usage limit, with room,
/// means it is working again (`waiting::note_reading`).
fn note_session_working(
    store: &Store,
    input: &StatusInput,
    account_id: i64,
    sample: &Sample,
    now_ms: i64,
) -> Result<()> {
    match input.session_id.as_deref() {
        Some(session) if !session.is_empty() => {
            crate::waiting::note_reading(store, session, account_id, sample, now_ms)
        }
        _ => Ok(()),
    }
}

/// How often a swap away from paid extra usage is asked for while one is on its way.
const AVOID_EVERY_MS: i64 = 30_000;

/// `extra_usage::avoid_swap`, at most every `AVOID_EVERY_MS`: every session's tick would ask
/// otherwise, while the swap it asked for is still running.
fn avoid_extra_usage(
    store: &Store,
    pool: &[scheduler::Candidate],
    policy: scheduler::Policy,
    active: &Account,
    now_ms: i64,
) -> Result<Option<SwapRequest>> {
    if store
        .meta_i64(META_SWAP_REQUESTED_AT)?
        .is_some_and(|t| (0..AVOID_EVERY_MS).contains(&(now_ms - t)))
    {
        return Ok(None);
    }
    crate::extra_usage::avoid_swap(store, pool, policy, active, now_ms / 1000)
}

/// When a swap stopped half way, settling it runs Claude Code: ask at most once a minute (one of
/// the processes noticing it at once does).
const SETTLE_EVERY_MS: i64 = 60_000;
const META_SETTLE_AT: &str = "dead_swap_settle_at";

fn settle_due(store: &Store, now_ms: i64) -> Result<bool> {
    store.immediate(|s| {
        let due = s
            .meta_i64(META_SETTLE_AT)?
            .is_none_or(|t| now_ms - t >= SETTLE_EVERY_MS);
        if due {
            s.set_meta(META_SETTLE_AT, now_ms)?;
        }
        Ok(due)
    })
}

/// Whether the profile's login has to be identified (a swap stopped half way, or the login
/// changed without cc-rotate writing it) and it is time to try again (`swap::settle_profile`
/// with the swap lock, which callers run off the hot path).
pub fn needs_settling(store: &Store, paths: &Paths, now_ms: i64) -> Result<bool> {
    // The store only, no credentials: the app asks every few seconds, and reading the macOS
    // Keychain starts a process. The write-back leaves the mark of a changed login.
    if crate::lock::is_held(&paths.swap_lock(), crate::lock::CLAUDE_STALE)
        || (swap::pending_swap(store)?.is_none()
            && store.meta(swap::META_LOGIN_TO_IDENTIFY)?.is_none())
    {
        return Ok(false);
    }
    settle_due(store, now_ms)
}

/// Readings dropped as another account's may be the active account's after all, if the reset
/// times known to be its own are wrong. When such a reading is at a limit and no reading of the
/// active account itself came for `CONFIRM_QUIET_MS`, ask for a usage check of it (at most that
/// often): its result is the account's own reading, which the next tick acts on.
fn confirm_due(store: &Store, sample: &Sample, active_id: i64, now_ms: i64) -> Result<bool> {
    let settings = store.settings()?;
    if !settings.usage_checks {
        return Ok(false);
    }
    let models = swap::models_in_use(store)?;
    let thresholds = scheduler::Thresholds::from(&settings);
    let thresholds = match store.account(active_id)? {
        Some(account) => thresholds.for_account(&account),
        None => thresholds,
    };
    let blocking = scheduler::window_usage(Some(sample), now_ms / 1000, thresholds, &models)
        .iter()
        .any(scheduler::WindowUsage::blocking);
    if !blocking {
        return Ok(false);
    }
    // The first such reading since the account became active asks at once, whatever it is: at a
    // limit, a genuine reading can repeat another account's exactly (both at 100% with the same
    // reset time), and only the account's own reading tells. After that, only a reading that is
    // not a copy of another account's latest asks, after a quiet spell: a session idle since an
    // earlier swap repeats that account's reading for as long as it idles, and asking again
    // would not change anything.
    let since = swap::active_since(store)?;
    let frozen = swap::copies_another(sample, &swap::others_latest(store, active_id)?);
    let quiet = store
        .last_sample(active_id)?
        .is_none_or(|s| now_ms - s.ts > CONFIRM_QUIET_MS);
    // One of the sessions ticking at once asks.
    store.immediate(|s| {
        let last = s.meta_i64(META_CONFIRM_REQUESTED_AT)?;
        let first = last.is_none_or(|t| t < since);
        let again = !frozen && quiet && last.is_none_or(|t| now_ms - t >= CONFIRM_QUIET_MS);
        let ask = first || again;
        if ask {
            s.set_meta(META_CONFIRM_REQUESTED_AT, now_ms)?;
        }
        Ok(ask)
    })
}

/// The throttling state a decision needs: the last swap, the last request, and whether the
/// active account was already over a threshold when it became active.
pub fn timing(store: &Store, active_id: i64, now_ms: i64) -> Result<Timing> {
    let settings = store.settings()?;
    let mut timing = Timing {
        now_ms,
        last_swap_ms: store.meta_i64(META_LAST_SWAP_AT)?.unwrap_or(0),
        last_request_ms: store.meta_i64(META_SWAP_REQUESTED_AT)?.unwrap_or(0),
        cooldown_ms: settings.swap_cooldown_s * 1000,
        arrived_over_limit: false,
    };
    // Only matters during the cooldown, which keeps the readings to look at few.
    if timing.cooling_down() {
        timing.arrived_over_limit = swap::arrived_over_limit(store, active_id, now_ms)?;
    }
    Ok(timing)
}

/// Observe-only tick for a profile outside the pool: record a sample for whichever pool
/// account that profile is signed in as. Never touches credentials or swaps. `home` is where
/// the default profile (`~/.claude`) keeps its `.claude.json`.
pub fn observe(
    store: &Store,
    profile_dir: &Path,
    home: &Path,
    input: &StatusInput,
    now_ms: i64,
) -> Result<Option<Account>> {
    let config = crate::profiles::config_file_for(profile_dir, home);
    let Some(ident) = profile_account::read(&config)? else {
        return Ok(None);
    };
    let account = match (&ident.account_uuid, &ident.email) {
        (Some(uuid), _) if store.account_by_uuid(uuid)?.is_some() => store.account_by_uuid(uuid)?,
        (_, Some(email)) => store.account_by_email(email)?,
        _ => None,
    };
    // Limits are per organization, and one person can be in several with the same account id
    // and email (a personal plan and a Team seat): only count the profile for the pool account
    // when it is signed in to that account's organization.
    let Some(account) = account.filter(|a| a.org_uuid.is_some() && a.org_uuid == ident.org_uuid)
    else {
        return Ok(None);
    };
    let profile = profile_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some(sample) = input.sample_for(account.id, &profile, now_ms) {
        store.record_sample(&sample)?;
        // A profile that never swaps only ever shows its own account.
        swap::note_known_resets(store, account.id, &sample.windows, now_ms / 1000)?;
    }
    let process = process_key(store, input, now_ms)?;
    note_cost(store, input, &process, account.id, now_ms)?;
    Ok(Some(account))
}

/// The Claude Code process that runs the session: two can run one session (resumed twice), each
/// with its own numbers and cost. Claude Code names itself to the commands it runs
/// (`CLAUDE_PID`, `StatusInput::claude_pid`), which tells its processes apart exactly. Without
/// that, it is told by when it started, from how long it has run (`Store::session_process`): the
/// status line runs under a process of its own each time, so its parent is no guide. A process
/// that says neither shares one key with its session.
fn process_key(store: &Store, input: &StatusInput, now_ms: i64) -> Result<String> {
    let Some(session) = input.session_id.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(String::new());
    };
    if let Some(pid) = input
        .claude_pid
        .filter(|_| names_its_process(input.version.as_deref()))
    {
        return Ok(format!("{NAMED_PROCESS_PREFIX}{pid}"));
    }
    match ran_ms(input) {
        Some(ran) => store.session_process(session, now_ms - ran, now_ms),
        None => Ok(String::new()),
    }
}

/// How long the session's process has run (`CostInfo::total_duration_ms`), in ms.
fn ran_ms(input: &StatusInput) -> Option<i64> {
    input
        .cost
        .as_ref()
        .and_then(|c| c.total_duration_ms)
        .filter(|d| d.is_finite() && *d >= 0.0)
        .map(|d| d as i64)
}

/// Whether a Claude Code of this version sets `CLAUDE_PID` itself (F25), so the one the status
/// line sees is its own and not one an older Claude Code inherited from where it was started.
fn names_its_process(version: Option<&str>) -> bool {
    version
        .and_then(herdr::parse_version)
        .is_some_and(|v| v >= CLAUDE_PID_SINCE)
}

/// The oldest Claude Code release seen setting `CLAUDE_PID` for its status line (F25).
const CLAUDE_PID_SINCE: (u64, u64, u64) = (2, 1, 316);

/// The Claude Code process a status line or hook runs for: Claude Code sets `CLAUDE_PID` to its
/// own process id for every command it runs, replacing one it inherited (seen on Claude Code 2.1.316 and 2.1.318).
pub fn claude_pid_from_env() -> Option<u32> {
    claude_pid(std::env::var("CLAUDE_PID").ok().as_deref())
}

fn claude_pid(value: Option<&str>) -> Option<u32> {
    value?.trim().parse().ok().filter(|pid| *pid > 0)
}

/// Whether the session has reported these numbers unchanged since before the account in use
/// became active (`Store::note_session_reading`): it has had no response since, so they are the
/// account it used before. A session only notices a new login when it next starts a request
/// (F1), and repeats its last numbers until then, however long it idles (F24).
fn idle_since_swap(
    store: &Store,
    input: &StatusInput,
    process: &str,
    sample: &Sample,
    now_ms: i64,
) -> Result<bool> {
    let Some(session) = input.session_id.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(false);
    };
    let since = store.note_session_reading(session, process, &sample.windows, now_ms)?;
    if since >= swap::active_since(store)? {
        return Ok(false);
    }
    // The numbers the active account was last read at: a session swapped back to an account
    // whose numbers did not change while it was parked gets them again from its first response.
    let active_latest = store.last_sample(sample.account_id)?;
    Ok(!active_latest.is_some_and(|l| crate::store::same_windows(&l.windows, &sample.windows)))
}

/// Count what the session's use cost since its last reading, at API prices, for `account_id`
/// (`Store::note_session_cost`).
fn note_cost(
    store: &Store,
    input: &StatusInput,
    process: &str,
    account_id: i64,
    now_ms: i64,
) -> Result<()> {
    let Some(cost) = input.cost.as_ref() else {
        return Ok(());
    };
    let (Some(session), Some(total)) = (input.session_id.as_deref(), cost.total_cost_usd) else {
        return Ok(());
    };
    if session.is_empty() {
        return Ok(());
    }
    // Two processes can run one session (resumed twice, each restoring the same total): each
    // keeps its own count (`process_key`).
    match input.launch_dir() {
        Some(dir) => store.note_session_cost_for_project(
            session,
            process,
            total,
            ran_ms(input),
            account_id,
            now_ms,
            dir,
        )?,
        None => {
            store.note_session_cost(session, process, total, ran_ms(input), account_id, now_ms)?
        }
    };
    Ok(())
}

/// The account a reading that is not the account in use's belongs to: one whose latest reading
/// it repeats exactly, else the one whose latest reading shares the most windows' reset times
/// with it, else the account the last swap left.
fn reading_account(store: &Store, sample: &Sample, active_id: i64) -> Result<Option<i64>> {
    let mut best: Option<(usize, i64)> = None;
    for account in store.accounts()? {
        if account.id == active_id {
            continue;
        }
        let Some(latest) = store.last_sample(account.id)? else {
            continue;
        };
        if crate::store::same_windows(&sample.windows, &latest.windows) {
            return Ok(Some(account.id));
        }
        let shared = sample
            .windows
            .iter()
            .filter(|w| {
                w.resets_at.is_some_and(|r| {
                    latest.windows.iter().any(|o| {
                        o.name == w.name && o.resets_at.is_some_and(|or| swap::same_reset(or, r))
                    })
                })
            })
            .count();
        if shared > 0 && best.is_none_or(|(n, _)| shared > n) {
            best = Some((shared, account.id));
        }
    }
    match best {
        Some((_, id)) => Ok(Some(id)),
        None => left_account(store),
    }
}

/// The account the last swap left, while it is in the pool.
fn left_account(store: &Store) -> Result<Option<i64>> {
    Ok(store
        .last_event_of(kind::SWAP)?
        .and_then(|e| e.account_from)
        .filter(|&id| store.account(id).ok().flatten().is_some()))
}

/// Shortly after a swap, a session that has not made a request yet still reports the old
/// account's limits. Those readings carry the old account's reset times; drop them.
fn is_stale_reading(store: &Store, sample: &Sample, now_ms: i64) -> Result<bool> {
    let last_swap = store.meta_i64(META_LAST_SWAP_AT)?.unwrap_or(0);
    if now_ms - last_swap > STALE_SAMPLE_GUARD_MS {
        return Ok(false);
    }
    let old = swap::left_resets(store)?;
    Ok(sample.windows.iter().any(|w| {
        w.resets_at
            .is_some_and(|r| old.iter().any(|&o| swap::same_reset(o, r)))
    }))
}

/// Write-back outside a statusline tick, for callers that run while no interactive session
/// does (the desktop app's watcher, `cc-rotate-ctl status`). It also records the usage the
/// user's other profiles read (`profile_usage`), which the tick leaves out to stay within its
/// time budget. Returns a swap request when the profile's login turned out to be dead, or when
/// an account without a weekly limit or an earlier expiring week is ready (the same rules and throttling
/// as a tick,
/// but no threshold triggers, since those need a live reading).
pub fn sync(store: &Store, paths: &Paths) -> Result<Option<SwapRequest>> {
    sync_at(store, paths, crate::time::now_ms())
}

pub fn sync_at(store: &Store, paths: &Paths, now_ms: i64) -> Result<Option<SwapRequest>> {
    let mut dead_login = false;
    if !sync_profile_login(store, paths, &mut dead_login, &mut false)? {
        // Settled where it can take its time (`needs_settling`).
        return Ok(None);
    }
    // Best effort, like before a swap: an unreadable profile must not stop the write-back.
    let _ = crate::profile_usage::import(store, paths, now_ms);
    let Some(active) = store.active_account()? else {
        return Ok(None);
    };
    let (pool, policy) = swap::load_pool(store, now_ms / 1000)?;
    crate::budget::note_in_use(store, &pool)?;
    let timing = timing(store, active.id, now_ms)?;
    // The app runs this when no session ticks, as when every session waits at a limit.
    if !dead_login && let Some(req) = avoid_extra_usage(store, &pool, policy, &active, now_ms)? {
        store.set_meta(META_SWAP_REQUESTED_AT, now_ms)?;
        return Ok(Some(req));
    }
    let Some(reason) = scheduler::decide(&pool, policy, timing, dead_login, false) else {
        return Ok(None);
    };
    store.set_meta(META_SWAP_REQUESTED_AT, now_ms)?;
    Ok(Some(SwapRequest {
        from: Some(active.id),
        to: None,
        reason,
        wait: false,
    }))
}

/// Keep the store's copy of the profile's login current, and notice when the login in the
/// profile changed for reasons other than our own swaps. The profile's login is the active
/// account's, or the home account's in relay mode (`relay::profile_account`).
///
/// A dead login sets `dead_login`, so the tick asks for a swap away from the account. In relay
/// mode, when the dead login is the home account's and another account is active, the home
/// account is marked signed out instead: no swap can fix it, since every session needs it.
/// Returns false when nothing could be synced because a swap stopped half way and the profile's
/// login is not identified yet. Sets `unidentified` when the login changed without cc-rotate
/// writing it and waits for Claude Code to say whose it is (`swap::identify_changed_login`).
fn sync_profile_login(
    store: &Store,
    paths: &Paths,
    dead_login: &mut bool,
    unidentified: &mut bool,
) -> Result<bool> {
    if swap::finish_dead_swap(store, paths, swap::Settler::Tick)? == swap::Settle::Unsure {
        return Ok(false);
    }
    if adopt_manual_login(store, paths)? {
        return Ok(true);
    }

    let creds = paths.credentials();
    let Some(marker) = creds.change_marker()? else {
        return Ok(true);
    };
    if store.meta(META_CRED_SEEN)?.as_deref() == Some(marker.as_str()) {
        return Ok(true);
    }
    let Some(oauth) = creds.read_oauth()? else {
        store.set_meta(META_CRED_SEEN, &marker)?;
        return Ok(true);
    };
    // A swap writes the target's login before it commits; until then the store still names the
    // outgoing account as active, and the login in the file is not the outgoing account's. The
    // file is read first and the pending swap second: once the pending mark is gone, the commit
    // is done and the active account read next is the target. Leave the change unseen.
    let file_fp = credfile::oauth_fingerprint(&oauth);
    if file_fp.is_some() && swap::pending_swap(store)?.map(|p| p.fp) == file_fp {
        return Ok(true);
    }
    let Some(owner) = relay::profile_account(store)? else {
        store.set_meta(META_CRED_SEEN, &marker)?;
        return Ok(true);
    };

    if credfile::is_dead(&oauth) {
        let home_only = relay::home(store)?.is_some()
            && store.active_account()?.is_none_or(|a| a.id != owner.id);
        if store.meta(META_DEAD_LOGIN_SEEN)?.as_deref() != Some(marker.as_str()) {
            store.log_event(
                kind::DEAD_LOGIN,
                Some(owner.id),
                None,
                Some("refresh_rejected"),
                None,
            )?;
            store.set_meta(META_DEAD_LOGIN_SEEN, &marker)?;
        }
        if home_only {
            // `doctor` reports it until the home account is signed in again.
            if owner.state != AccountState::NeedsLogin {
                store.set_state(owner.id, AccountState::NeedsLogin)?;
            }
        } else {
            *dead_login = true;
        }
        // Leave the change unseen so every tick keeps requesting a swap until one lands, or, for
        // the home account, until it is signed in again.
        return Ok(true);
    }

    // A swap may have written another login since the file was read (and committed, so the
    // active account read above is already the new one): then this login is not that account's.
    if creds.change_marker()?.as_deref() != Some(marker.as_str()) {
        return Ok(true);
    }
    let stored_fp = store.credential_meta(owner.id)?.map(|m| m.refresh_fp);
    if file_fp.is_some() && file_fp != stored_fp {
        // A login cc-rotate did not write: a renewal of the profile's account's, or a `/login` that
        // `.claude.json` does not reveal after a swap (F14). Left unseen until Claude Code says
        // whose it is, unless logins are not identified.
        if crate::usage_check::identifies_logins(store, paths)? {
            *unidentified = true;
            if store.meta(swap::META_LOGIN_TO_IDENTIFY)?.as_deref() != Some(marker.as_str()) {
                store.set_meta(swap::META_LOGIN_TO_IDENTIFY, &marker)?;
                if store.meta(swap::META_LOGIN_CHANGED_AT)?.is_none() {
                    store.set_meta(swap::META_LOGIN_CHANGED_AT, crate::time::now_ms())?;
                }
            }
            return Ok(true);
        }
        swap::store_profile_login(store, owner.id, &oauth)?;
    }
    store.set_meta(META_CRED_SEEN, &marker)?;
    Ok(true)
}

/// If someone ran `/login` inside the rotation profile, `.claude.json` names a different
/// account than the profile's (`relay::profile_account`). When that account is in the pool, make
/// it active (the home account in relay mode, `swap::adopt_login`) and store its new login; when
/// it is not, record the foreign login once and stop syncing until it changes back.
///
/// Returns true when the tick must not sync the credentials this time.
fn adopt_manual_login(store: &Store, paths: &Paths) -> Result<bool> {
    let foreign = store.meta(META_PROFILE_FOREIGN)?.as_deref() == Some("1");
    let config = paths.profile_config();
    let Some(mtime) = credfile::mtime_ms(&config) else {
        return Ok(foreign);
    };
    if store.meta_i64(META_CONFIG_MTIME_SEEN)? == Some(mtime) {
        return Ok(foreign);
    }
    store.set_meta(META_CONFIG_MTIME_SEEN, mtime)?;

    let Some(uuid) = profile_account::read(&config)?.and_then(|a| a.account_uuid) else {
        return Ok(foreign);
    };
    let previous = store.meta(META_PROFILE_UUID_SEEN)?;
    if previous.as_deref() == Some(uuid.as_str()) {
        return Ok(foreign);
    }
    store.set_meta(META_PROFILE_UUID_SEEN, &uuid)?;
    store.set_meta(META_PROFILE_FOREIGN, "0")?;
    if previous.is_none() {
        return Ok(false);
    }

    let owner = relay::profile_account(store)?;
    if owner.as_ref().and_then(|a| a.account_uuid.as_deref()) == Some(uuid.as_str()) {
        return Ok(false);
    }

    match store.account_by_uuid(&uuid)? {
        Some(account) => {
            let creds = paths.credentials();
            let oauth = creds.read_oauth()?;
            // `/login` wrote `.claude.json` when it changed the login.
            let changed_at = mtime.min(crate::time::now_ms());
            store.immediate(|s| {
                swap::adopt_login(s, owner.as_ref(), &account, oauth.as_ref(), changed_at)
            })?;
            if let Some(marker) = creds.change_marker()? {
                store.set_meta(META_CRED_SEEN, &marker)?;
            }
            Ok(true)
        }
        None => {
            store.log_event(
                kind::FOREIGN_LOGIN,
                owner.as_ref().map(|a| a.id),
                None,
                Some("login_in_profile"),
                Some(json!({ "account_uuid": uuid })),
            )?;
            store.set_meta(META_PROFILE_FOREIGN, "1")?;
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_statusline_payload() {
        let input = StatusInput::parse(
            r#"{"session_id":"s1","model":{"id":"claude-opus-4-1","display_name":"Opus"},
                "rate_limits":{"five_hour":{"used_percentage":41.5,"resets_at":1792890000},
                "seven_day":{"used_percentage":12,"resets_at":"2026-10-30T00:00:00Z"}}}"#,
        );
        assert_eq!(input.model_id(), Some("claude-opus-4-1"));
        let rl = input.rate_limits.as_ref().unwrap();
        assert_eq!(rl["five_hour"].used_percentage, Some(41.5));
        assert_eq!(rl["five_hour"].resets_at, Some(1_792_890_000));
        assert_eq!(rl["seven_day"].resets_at, Some(1_793_318_400));
    }

    #[test]
    fn launch_dir_is_the_folder_claude_code_was_started_in() {
        let launched = StatusInput::parse(
            r#"{"cwd":"/now","workspace":{"project_dir":"/work/app","current_dir":"/now"}}"#,
        );
        assert_eq!(launched.launch_dir(), Some("/work/app"));
        let cwd_only = StatusInput::parse(r#"{"cwd":"/work/app"}"#);
        assert_eq!(cwd_only.launch_dir(), Some("/work/app"));
        let current = StatusInput::parse(r#"{"workspace":{"current_dir":"/work/app"}}"#);
        assert_eq!(current.launch_dir(), Some("/work/app"));
        let blank = StatusInput::parse(r#"{"cwd":"  ","workspace":{"project_dir":""}}"#);
        assert_eq!(blank.launch_dir(), None);
    }

    #[test]
    fn records_every_window_it_is_given() {
        let input = StatusInput::parse(
            r#"{"rate_limits":{"five_hour":{"used_percentage":5,"resets_at":100},
                "seven_day_opus":{"used_percentage":70,"resets_at":200},
                "spend_limit":{"used_percentage":1,"resets_at":300,"used_usd":2},
                "not_a_window":"text","no_reading":{"resets_at":400}}}"#,
        );
        let sample = input.sample_for(1, "rotate", 0).unwrap();
        let names: Vec<_> = sample.windows.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, vec!["five_hour", "seven_day_opus", "spend_limit"]);
        assert_eq!(sample.window("seven_day_opus").unwrap().pct, 70.0);

        // A Team seat: only the 5-hour window.
        let team = StatusInput::parse(
            r#"{"rate_limits":{"five_hour":{"used_percentage":5,"resets_at":100}}}"#,
        );
        assert_eq!(team.sample_for(1, "rotate", 0).unwrap().windows.len(), 1);
        assert!(
            StatusInput::parse(r#"{"rate_limits":{}}"#)
                .sample_for(1, "rotate", 0)
                .is_none()
        );
    }

    #[test]
    fn claude_pid_is_a_process_id() {
        assert_eq!(claude_pid(Some("39004")), Some(39004));
        assert_eq!(claude_pid(Some(" 7 ")), Some(7));
        for bad in [
            None,
            Some(""),
            Some("0"),
            Some("-5"),
            Some("abc"),
            Some("1.5"),
        ] {
            assert_eq!(claude_pid(bad), None, "{bad:?}");
        }
        // Never read from the JSON.
        assert_eq!(StatusInput::parse(r#"{"claude_pid":5}"#).claude_pid, None);
    }

    #[test]
    fn only_a_claude_code_that_sets_claude_pid_is_keyed_by_it() {
        for (version, named) in [
            (Some("2.1.316"), true),
            (Some("2.1.318"), true),
            (Some("2.1.319 (Claude Code)"), true),
            (Some("2.2.0"), true),
            (Some("3.0.0"), true),
            (Some("2.1.315"), false),
            (Some("1.9.999"), false),
            (Some("garbage"), false),
            (None, false),
        ] {
            assert_eq!(names_its_process(version), named, "{version:?}");
        }
    }

    #[test]
    fn tolerates_garbage() {
        assert!(StatusInput::parse("not json").rate_limits.is_none());
        assert!(StatusInput::parse("{}").rate_limits.is_none());
    }

    mod relay_mode {
        use super::super::*;
        use crate::relay::{self, RelayConfig};
        use crate::store::AccountState;
        use crate::swap::tests::{Env, oauth};

        const PORT: u16 = 51234;
        const SECRET: &str = "0123456789abcdef0123456789abcdef";

        /// Relay mode on with `a` as home and `b` active, `a`'s login in the profile, and
        /// `relay.json` written.
        fn relay_env() -> Env {
            let env = home_active_env();
            env.activate(env.b);
            env
        }

        /// Relay mode on with `a` as home and active, and `relay.json` written.
        fn home_active_env() -> Env {
            let env = Env::new();
            relay::set_mode(&env.store, Some(env.a)).unwrap();
            std::fs::write(
                env.paths.relay_config(),
                serde_json::to_vec(&RelayConfig {
                    port: PORT,
                    secret: SECRET.into(),
                })
                .unwrap(),
            )
            .unwrap();
            env
        }

        /// The environment of a session started through the relay.
        fn through_relay(name: &str) -> Option<String> {
            let cfg = RelayConfig {
                port: PORT,
                secret: SECRET.into(),
            };
            (name == relay::ENV_BASE_URL).then(|| cfg.base_url())
        }

        fn no_env(_: &str) -> Option<String> {
            None
        }

        fn input(session: &str, five_hour: f64, cost: f64) -> StatusInput {
            StatusInput::parse(&format!(
                r#"{{"session_id":"{session}","model":{{"id":"claude-opus-4-1"}},
                    "version":"2.1.320","cost":{{"total_cost_usd":{cost}}},
                    "rate_limits":{{"five_hour":{{"used_percentage":{five_hour},"resets_at":{}}}}}}}"#,
                crate::time::now_s() + 3600
            ))
        }

        #[test]
        fn relay_tick_records_no_windows() {
            let env = relay_env();
            let now = crate::time::now_ms();
            tick(
                &env.store,
                &env.paths,
                &input("s1", 40.0, 1.0),
                now,
                through_relay,
            )
            .unwrap();
            tick(
                &env.store,
                &env.paths,
                &input("s1", 41.0, 3.0),
                now + 1000,
                through_relay,
            )
            .unwrap();
            assert!(env.store.last_sample(env.a).unwrap().is_none());
            assert!(env.store.last_sample(env.b).unwrap().is_none());
            // The model and the cost are still recorded, the cost for the active account.
            assert_eq!(
                env.store.meta(META_PROFILE_MODEL).unwrap().as_deref(),
                Some("claude-opus-4-1")
            );
            let (from, to) = (now / 1000 - 7200, now / 1000 + 7200);
            assert!(
                env.store
                    .account_cost_between(env.b, from, to)
                    .unwrap()
                    .is_some_and(|c| c > 0.0)
            );
            assert_eq!(
                env.store.account_cost_between(env.a, from, to).unwrap(),
                None
            );
        }

        #[test]
        fn relay_tick_no_threshold_swap() {
            let env = relay_env();
            let now = crate::time::now_ms();
            env.store
                .record_sample(&Sample {
                    ts: now - 1000,
                    account_id: env.b,
                    profile: "relay".into(),
                    session_id: None,
                    model: None,
                    windows: vec![SampleWindow {
                        name: "five_hour".into(),
                        pct: 99.0,
                        resets_at: Some(now / 1000 + 3600),
                    }],
                })
                .unwrap();
            let r = tick(
                &env.store,
                &env.paths,
                &input("s1", 99.0, 1.0),
                now,
                through_relay,
            )
            .unwrap();
            assert!(r.swap.is_none(), "{:?}", r.swap);
            assert!(!r.confirm);
        }

        #[test]
        fn relay_dead_home_login_marks_home() {
            let env = relay_env();
            let now = crate::time::now_ms();
            env.write_profile(&serde_json::json!({
                "accessToken": "", "refreshToken": "", "expiresAt": 0
            }));
            let r = tick(
                &env.store,
                &env.paths,
                &StatusInput::default(),
                now,
                through_relay,
            )
            .unwrap();
            assert!(r.swap.is_none(), "{:?}", r.swap);
            assert_eq!(env.state(env.a), AccountState::NeedsLogin);
            assert_eq!(env.state(env.b), AccountState::Active);
            let event = env.store.last_event_of(kind::DEAD_LOGIN).unwrap().unwrap();
            assert_eq!(event.account_from, Some(env.a));
            // The next tick neither asks for a swap nor logs it again.
            let r = tick(
                &env.store,
                &env.paths,
                &StatusInput::default(),
                now + 1000,
                through_relay,
            )
            .unwrap();
            assert!(r.swap.is_none());
            assert_eq!(env.state(env.b), AccountState::Active);
        }

        #[test]
        fn relay_dead_login_of_an_active_home_asks_for_a_swap() {
            let env = home_active_env();
            env.write_profile(&serde_json::json!({
                "accessToken": "", "refreshToken": "", "expiresAt": 0
            }));
            let r = tick(
                &env.store,
                &env.paths,
                &StatusInput::default(),
                crate::time::now_ms(),
                through_relay,
            )
            .unwrap();
            let req = r
                .swap
                .expect("a dead login of the account in use asks for a swap");
            assert_eq!(req.reason, Reason::DeadLogin);
            assert_eq!(req.from, Some(env.a));
        }

        #[test]
        fn relay_relogin_of_home_clears_needs_login() {
            let env = relay_env();
            let now = crate::time::now_ms();
            env.write_profile(&serde_json::json!({
                "accessToken": "", "refreshToken": "", "expiresAt": 0
            }));
            tick(
                &env.store,
                &env.paths,
                &StatusInput::default(),
                now,
                through_relay,
            )
            .unwrap();
            assert_eq!(env.state(env.a), AccountState::NeedsLogin);
            // The user signs in to the home account again in the profile.
            env.write_profile(&oauth("a2"));
            let r = tick(
                &env.store,
                &env.paths,
                &StatusInput::default(),
                now + 1000,
                through_relay,
            )
            .unwrap();
            assert!(r.swap.is_none(), "{:?}", r.swap);
            assert_eq!(env.state(env.a), AccountState::Parked);
            assert_eq!(env.stored_refresh(env.a), "refresh-a2");
            assert_eq!(env.state(env.b), AccountState::Active);
        }

        #[test]
        fn relay_bypass_cost_goes_to_home() {
            let env = relay_env();
            let now = crate::time::now_ms();
            for (i, cost) in [1.0, 3.0].into_iter().enumerate() {
                tick(
                    &env.store,
                    &env.paths,
                    &input("bypass", 10.0, cost),
                    now + i as i64 * 1000,
                    no_env,
                )
                .unwrap();
            }
            let (from, to) = (now / 1000 - 7200, now / 1000 + 7200);
            assert!(
                env.store
                    .account_cost_between(env.a, from, to)
                    .unwrap()
                    .is_some_and(|c| c > 0.0)
            );
            assert_eq!(
                env.store.account_cost_between(env.b, from, to).unwrap(),
                None
            );
        }

        #[test]
        fn relay_writeback_to_home() {
            let env = relay_env();
            let now = crate::time::now_ms();
            tick(
                &env.store,
                &env.paths,
                &StatusInput::default(),
                now,
                through_relay,
            )
            .unwrap();
            // Claude Code renewed the home account's login in the profile.
            env.write_profile(&oauth("a2"));
            tick(
                &env.store,
                &env.paths,
                &StatusInput::default(),
                now + 1000,
                through_relay,
            )
            .unwrap();
            assert_eq!(env.stored_refresh(env.a), "refresh-a2");
            assert_eq!(env.stored_refresh(env.b), "refresh-b1");
            assert_eq!(env.state(env.b), AccountState::Active);
        }

        #[test]
        fn relay_login_of_the_active_account_makes_it_home() {
            let env = relay_env();
            let config = env.paths.profile_config();
            let write_config = |uuid: &str| {
                std::fs::write(
                    &config,
                    serde_json::json!({"oauthAccount": {"accountUuid": uuid}}).to_string(),
                )
                .unwrap();
            };
            write_config("ua");
            let now = crate::time::now_ms();
            tick(
                &env.store,
                &env.paths,
                &StatusInput::default(),
                now,
                through_relay,
            )
            .unwrap();
            // A `/login` as `b`, the active account, inside the profile.
            std::thread::sleep(std::time::Duration::from_millis(20));
            env.write_profile(&oauth("b-manual"));
            write_config("ub");
            tick(
                &env.store,
                &env.paths,
                &StatusInput::default(),
                now + 1000,
                through_relay,
            )
            .unwrap();
            assert_eq!(relay::home(&env.store).unwrap(), Some(env.b));
            assert_eq!(env.stored_refresh(env.b), "refresh-b-manual");
            assert_eq!(env.stored_refresh(env.a), "refresh-a1");
            assert_eq!(env.state(env.b), AccountState::Active);
            let event = env
                .store
                .last_event_of(kind::ADOPTED_LOGIN)
                .unwrap()
                .unwrap();
            assert_eq!(event.account_from, Some(env.a));
        }

        #[test]
        fn relay_bypass_counted() {
            let env = relay_env();
            let now = crate::time::now_ms();
            tick(
                &env.store,
                &env.paths,
                &input("through", 10.0, 1.0),
                now,
                through_relay,
            )
            .unwrap();
            assert_eq!(relay::bypass_count(&env.store, now).unwrap(), 0);
            tick(
                &env.store,
                &env.paths,
                &input("bypass", 10.0, 1.0),
                now,
                no_env,
            )
            .unwrap();
            let other_url = |name: &str| {
                (name == relay::ENV_BASE_URL).then(|| "https://api.anthropic.com".to_string())
            };
            tick(
                &env.store,
                &env.paths,
                &input("elsewhere", 10.0, 1.0),
                now,
                other_url,
            )
            .unwrap();
            assert_eq!(relay::bypass_count(&env.store, now).unwrap(), 2);
            assert_eq!(
                relay::bypass_count(&env.store, now + 10 * 60_000).unwrap(),
                0
            );
        }

        #[test]
        fn no_bypass_counted_outside_relay_mode() {
            let env = Env::new();
            let now = crate::time::now_ms();
            tick(&env.store, &env.paths, &input("s1", 10.0, 1.0), now, no_env).unwrap();
            assert_eq!(relay::bypass_count(&env.store, now).unwrap(), 0);
        }

        /// With relay mode off, a session started while it was on still sends its requests to
        /// the relay, which leaves an hour after `relay off` (spec 4).
        #[test]
        fn stale_relay_session_counted_outside_relay_mode() {
            let env = Env::new();
            let now = crate::time::now_ms();
            tick(
                &env.store,
                &env.paths,
                &input("plain", 10.0, 1.0),
                now,
                no_env,
            )
            .unwrap();
            assert_eq!(relay::stale_count(&env.store, now).unwrap(), 0);
            tick(
                &env.store,
                &env.paths,
                &input("stale", 10.0, 1.0),
                now,
                through_relay,
            )
            .unwrap();
            assert_eq!(relay::stale_count(&env.store, now).unwrap(), 1);
            assert_eq!(relay::bypass_count(&env.store, now).unwrap(), 0);
            assert_eq!(
                relay::stale_count(&env.store, now + 10 * 60_000).unwrap(),
                0
            );
        }

        #[test]
        fn no_stale_session_counted_in_relay_mode() {
            let env = relay_env();
            let now = crate::time::now_ms();
            tick(
                &env.store,
                &env.paths,
                &input("through", 10.0, 1.0),
                now,
                through_relay,
            )
            .unwrap();
            assert_eq!(relay::stale_count(&env.store, now).unwrap(), 0);
        }
    }
}
