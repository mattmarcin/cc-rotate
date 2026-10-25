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
use crate::scheduler::{self, Reason, Timing};
use crate::store::{Account, Sample, SampleWindow, Store, kind};
use crate::swap::{
    self, META_CRED_SEEN, META_LAST_SWAP_AT, META_PROFILE_FOREIGN, META_PROFILE_MODEL,
    STALE_SAMPLE_GUARD_MS, SwapRequest,
};
use crate::{credfile, time};

pub const META_LAST_TICK_AT: &str = "statusline_last_tick_at";
/// When a tick last asked for a usage check of the account in use (`confirm_due`).
const META_CONFIRM_REQUESTED_AT: &str = "confirm_requested_at";
const CONFIRM_QUIET_MS: i64 = 10 * 60_000;
pub const META_LAST_TICK_HAD_USAGE: &str = "statusline_last_tick_had_usage";
const META_CONFIG_MTIME_SEEN: &str = "profile_config_mtime_seen";
const META_PROFILE_UUID_SEEN: &str = "profile_account_uuid_seen";
const META_SWAP_REQUESTED_AT: &str = "swap_requested_at";
/// The credentials change marker a dead login was last reported for, so it is logged once.
const META_DEAD_LOGIN_SEEN: &str = "dead_login_seen";

/// The parts of Claude Code's status line input cc-rotate reads.
#[derive(Debug, Default, Deserialize)]
pub struct StatusInput {
    pub session_id: Option<String>,
    pub model: Option<ModelInfo>,
    /// Every usage window Claude Code reports, by name (`five_hour`, `seven_day`, ...). Kept
    /// generic so a window Claude Code adds later is recorded without a code change; a window
    /// is only present while the API reports it and its reset has not passed.
    #[serde(default, deserialize_with = "lenient_windows")]
    pub rate_limits: Option<BTreeMap<String, Window>>,
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
        Some(Raw::Num(n)) => Some(n as i64),
        Some(Raw::Text(s)) => s
            .parse::<f64>()
            .ok()
            .map(|n| n as i64)
            .or_else(|| time::parse_rfc3339(&s)),
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
pub fn tick(store: &Store, paths: &Paths, input: &StatusInput, now_ms: i64) -> Result<TickResult> {
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

    if !sync_profile_login(store, paths, &mut dead_login, &mut unidentified)? {
        // Nothing can be trusted about the profile's login until the swap is settled.
        result.settle_login = settle_due(store, now_ms)?;
        result.active = store.active_account()?;
        return Ok(result);
    }
    if unidentified && !crate::lock::is_held(&paths.swap_lock(), crate::lock::CLAUDE_STALE) {
        result.settle_login = settle_due(store, now_ms)?;
    }

    let Some(active) = store.active_account()? else {
        return Ok(result);
    };

    if let Some(model) = input.model_id() {
        if store.meta(META_PROFILE_MODEL)?.as_deref() != Some(model) {
            store.set_meta(META_PROFILE_MODEL, model)?;
        }
    }
    if let Some(sample) = input.sample_for(active.id, "rotate", now_ms) {
        if !is_stale_reading(store, &sample, now_ms)? {
            match swap::attribute(store, &sample, active.id, now_ms / 1000)? {
                swap::Attribution::Own => {
                    store.record_sample(&sample)?;
                    swap::note_known_resets(store, active.id, &sample.windows, now_ms / 1000)?;
                }
                // Recorded, but it does not vouch for the account's reset times.
                swap::Attribution::Ambiguous => {
                    store.record_sample(&sample)?;
                }
                swap::Attribution::Elsewhere => {
                    result.confirm = confirm_due(store, &sample, active.id, now_ms)?;
                }
            }
        }
    }

    let (pool, policy) = swap::load_pool(store, now_ms / 1000)?;
    let timing = timing(store, active.id, now_ms)?;
    if let Some(reason) = scheduler::decide(&pool, policy, timing, dead_login, true) {
        if reason != Reason::PreferredAvailable {
            result.pool_exhausted = scheduler::pick(&pool, policy, Some(active.id)).is_none();
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
        result.pool_exhausted = scheduler::pick(&pool, policy, Some(active.id)).is_none();
    }

    store.prune(now_ms)?;
    result.active = store.active_account()?;
    Ok(result)
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
    if crate::lock::is_held(&paths.swap_lock(), crate::lock::CLAUDE_STALE)
        || (swap::pending_swap(store)?.is_none() && swap::changed_login(store, paths)?.is_none())
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
    let model = store.meta(META_PROFILE_MODEL)?;
    let blocking = scheduler::window_usage(
        Some(sample),
        now_ms / 1000,
        scheduler::Thresholds::from(&settings),
        model.as_deref(),
    )
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

/// The throttling state a decision needs: the last swap, the last request, how long the active
/// account has been active, and whether it was already over a threshold when it became active.
fn timing(store: &Store, active_id: i64, now_ms: i64) -> Result<Timing> {
    let settings = store.settings()?;
    let mut timing = Timing {
        now_ms,
        last_swap_ms: store.meta_i64(META_LAST_SWAP_AT)?.unwrap_or(0),
        active_since_ms: swap::active_since(store)?,
        last_request_ms: store.meta_i64(META_SWAP_REQUESTED_AT)?.unwrap_or(0),
        cooldown_ms: settings.swap_cooldown_s * 1000,
        dwell_ms: settings.switch_back_dwell_s * 1000,
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
    Ok(Some(account))
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
/// a better ranked account is usable again (the same switch-back rule and throttling as a tick,
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
    let timing = timing(store, active.id, now_ms)?;
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

/// Keep the store's copy of the active login current, and notice when the login in the
/// profile changed for reasons other than our own swaps.
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
    let Some(active) = store.active_account()? else {
        store.set_meta(META_CRED_SEEN, &marker)?;
        return Ok(true);
    };

    if credfile::is_dead(&oauth) {
        *dead_login = true;
        if store.meta(META_DEAD_LOGIN_SEEN)?.as_deref() != Some(marker.as_str()) {
            store.log_event(
                kind::DEAD_LOGIN,
                Some(active.id),
                None,
                Some("refresh_rejected"),
                None,
            )?;
            store.set_meta(META_DEAD_LOGIN_SEEN, &marker)?;
        }
        // Leave the change unseen so every tick keeps requesting a swap until one lands.
        return Ok(true);
    }

    // A swap may have written another login since the file was read (and committed, so the
    // active account read above is already the new one): then this login is not that account's.
    if creds.change_marker()?.as_deref() != Some(marker.as_str()) {
        return Ok(true);
    }
    let stored_fp = store.credential_meta(active.id)?.map(|m| m.refresh_fp);
    if file_fp.is_some() && file_fp != stored_fp {
        // A login cc-rotate did not write: a renewal of the active account's, or a `/login` that
        // `.claude.json` does not reveal after a swap (F14). Left unseen until Claude Code says
        // whose it is, unless logins are not identified.
        if crate::usage_check::identifies_logins(store, paths)? {
            *unidentified = true;
            return Ok(true);
        }
        store.save_credential(active.id, &oauth, "sync")?;
        store.log_event(kind::SYNC, Some(active.id), None, Some("refreshed"), None)?;
    }
    store.set_meta(META_CRED_SEEN, &marker)?;
    Ok(true)
}

/// If someone ran `/login` inside the rotation profile, `.claude.json` names a different
/// account. When that account is in the pool, make it active and store its new login; when it
/// is not, record the foreign login once and stop syncing until it changes back.
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

    let active = store.active_account()?;
    if active.as_ref().and_then(|a| a.account_uuid.as_deref()) == Some(uuid.as_str()) {
        return Ok(false);
    }

    match store.account_by_uuid(&uuid)? {
        Some(account) => {
            let creds = paths.credentials();
            let oauth = creds.read_oauth()?;
            store.immediate(|s| swap::adopt_login(s, active.as_ref(), &account, oauth.as_ref()))?;
            if let Some(marker) = creds.change_marker()? {
                store.set_meta(META_CRED_SEEN, &marker)?;
            }
            Ok(true)
        }
        None => {
            store.log_event(
                kind::FOREIGN_LOGIN,
                active.as_ref().map(|a| a.id),
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
    fn tolerates_garbage() {
        assert!(StatusInput::parse("not json").rate_limits.is_none());
        assert!(StatusInput::parse("{}").rate_limits.is_none());
    }
}
