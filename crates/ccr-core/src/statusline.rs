//! The statusline tick (spec section 8): record usage, write renewed logins back to the store,
//! detect dead or foreign logins, and decide whether a swap is needed.
//!
//! This runs after every reply and every 30 seconds in every session, so it must stay fast and
//! must never fail in a way that breaks the user's status line; callers log errors and carry on.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Deserializer};
use serde_json::json;

use crate::credfile;
use crate::paths::Paths;
use crate::profile_account;
use crate::scheduler::{self, Reason, Thresholds, Usage};
use crate::store::{Account, AccountState, Sample, Store, kind};
use crate::swap::{
    META_CRED_MTIME_SEEN, META_LAST_SWAP_AT, META_LAST_SWAP_FROM_FIVE_RESETS,
    META_LAST_SWAP_FROM_WEEK_RESETS, META_PROFILE_FOREIGN, SwapRequest,
};

pub const META_LAST_TICK_AT: &str = "statusline_last_tick_at";
pub const META_LAST_TICK_HAD_USAGE: &str = "statusline_last_tick_had_usage";
const META_CONFIG_MTIME_SEEN: &str = "profile_config_mtime_seen";
const META_PROFILE_UUID_SEEN: &str = "profile_account_uuid_seen";
const META_SWAP_REQUESTED_AT: &str = "swap_requested_at";
/// Window after a swap during which samples matching the outgoing account's reset times are
/// treated as stale readings of the old account.
const STALE_SAMPLE_GUARD_MS: i64 = 120_000;
/// Minimum gap between swap requests spawned by ticks.
const REQUEST_INTERVAL_MS: i64 = 30_000;

#[derive(Debug, Default, Deserialize)]
pub struct StatusInput {
    pub session_id: Option<String>,
    pub rate_limits: Option<RateLimits>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RateLimits {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
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
            .or_else(|| parse_iso8601(&s)),
        None => None,
    })
}

/// Minimal RFC 3339 parser (`2026-10-24T18:00:00Z`, optional fraction and offset).
fn parse_iso8601(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (
        n(0..4)?,
        n(5..7)?,
        n(8..10)?,
        n(11..13)?,
        n(14..16)?,
        n(17..19)?,
    );
    let mut rest = &s[19..];
    if let Some(stripped) = rest.strip_prefix('.') {
        let digits = stripped
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(stripped.len());
        rest = &stripped[digits..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            sign * (o[1..3].parse::<i64>().ok()? * 3600 + o[4..6].parse::<i64>().ok()? * 60)
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant's algorithm).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + se - offset)
}

impl StatusInput {
    pub fn parse(text: &str) -> Self {
        serde_json::from_str(text).unwrap_or_default()
    }

    fn sample_for(&self, account_id: i64, profile: &str, now_ms: i64) -> Option<Sample> {
        let rl = self.rate_limits.as_ref()?;
        let five = rl.five_hour.as_ref();
        let week = rl.seven_day.as_ref();
        if five.and_then(|w| w.used_percentage).is_none()
            && week.and_then(|w| w.used_percentage).is_none()
        {
            return None;
        }
        Some(Sample {
            ts: now_ms,
            account_id,
            profile: profile.to_string(),
            session_id: self.session_id.clone(),
            five_pct: five.and_then(|w| w.used_percentage),
            five_resets_at: five.and_then(|w| w.resets_at),
            week_pct: week.and_then(|w| w.used_percentage),
            week_resets_at: week.and_then(|w| w.resets_at),
        })
    }
}

#[derive(Debug, Default)]
pub struct TickResult {
    pub active: Option<Account>,
    pub usage: Option<Usage>,
    pub swap: Option<SwapRequest>,
    pub pool_exhausted: bool,
}

/// One statusline tick in the rotation profile.
pub fn tick(store: &Store, paths: &Paths, input: &StatusInput, now_ms: i64) -> Result<TickResult> {
    let mut result = TickResult::default();
    let mut dead_login = false;

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

    sync_profile_login(store, paths, &mut dead_login)?;

    let Some(active) = store.active_account()? else {
        return Ok(result);
    };

    if let Some(sample) = input.sample_for(active.id, "rotate", now_ms) {
        if !is_stale_reading(store, &sample, now_ms)? {
            store.record_sample(&sample)?;
        }
    }
    let usage = Usage::from_sample(store.last_sample(active.id)?.as_ref(), now_ms / 1000);
    result.usage = Some(usage);

    let thresholds = Thresholds::from(&store.settings()?);
    let reason = if dead_login {
        Some(Reason::DeadLogin)
    } else {
        scheduler::threshold_trigger(usage, thresholds)
    };

    if let Some(reason) = reason {
        result.pool_exhausted = crate::swap::next_target(store, Some(active.id))?.is_none();
        let last_request = store.meta_i64(META_SWAP_REQUESTED_AT)?.unwrap_or(0);
        let last_swap = store.meta_i64(META_LAST_SWAP_AT)?.unwrap_or(0);
        let cooldown_ms = store.settings()?.swap_cooldown_s * 1000;
        let throttled = now_ms - last_request < REQUEST_INTERVAL_MS
            || (reason.respects_cooldown() && now_ms - last_swap < cooldown_ms);
        if dead_login || !throttled {
            store.set_meta(META_SWAP_REQUESTED_AT, now_ms)?;
            result.swap = Some(SwapRequest {
                from: Some(active.id),
                to: None,
                reason,
                wait: false,
            });
        }
    }

    store.prune(now_ms)?;
    result.active = store.active_account()?;
    Ok(result)
}

/// Observe-only tick for a profile outside the pool: record a sample for whichever pool
/// account that profile is signed in as. Never touches credentials or swaps.
pub fn observe(
    store: &Store,
    profile_dir: &Path,
    input: &StatusInput,
    now_ms: i64,
) -> Result<Option<Account>> {
    let Some(ident) = profile_account::read(&profile_dir.join(".claude.json"))? else {
        return Ok(None);
    };
    let account = match (&ident.account_uuid, &ident.email) {
        (Some(uuid), _) if store.account_by_uuid(uuid)?.is_some() => store.account_by_uuid(uuid)?,
        (_, Some(email)) => store.account_by_email(email)?,
        _ => None,
    };
    let Some(account) = account else {
        return Ok(None);
    };
    let profile = profile_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some(sample) = input.sample_for(account.id, &profile, now_ms) {
        store.record_sample(&sample)?;
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
    let five = store
        .meta_i64(META_LAST_SWAP_FROM_FIVE_RESETS)?
        .unwrap_or(0);
    let week = store
        .meta_i64(META_LAST_SWAP_FROM_WEEK_RESETS)?
        .unwrap_or(0);
    let matches = |v: Option<i64>, old: i64| old != 0 && v == Some(old);
    Ok(matches(sample.five_resets_at, five) || matches(sample.week_resets_at, week))
}

/// Write-back outside a statusline tick, for callers that run while no interactive session
/// does (the desktop app's watcher, `cc-rotate-ctl status`). Returns a swap request when the
/// profile's login turned out to be dead.
pub fn sync(store: &Store, paths: &Paths) -> Result<Option<SwapRequest>> {
    let mut dead_login = false;
    sync_profile_login(store, paths, &mut dead_login)?;
    if !dead_login {
        return Ok(None);
    }
    let Some(active) = store.active_account()? else {
        return Ok(None);
    };
    Ok(Some(SwapRequest {
        from: Some(active.id),
        to: None,
        reason: Reason::DeadLogin,
        wait: false,
    }))
}

/// Keep the store's copy of the active login current, and notice when the login in the
/// profile changed for reasons other than our own swaps.
fn sync_profile_login(store: &Store, paths: &Paths, dead_login: &mut bool) -> Result<()> {
    if adopt_manual_login(store, paths)? {
        return Ok(());
    }

    let cred_path = paths.credentials_file();
    let Some(mtime) = credfile::mtime_ms(&cred_path) else {
        return Ok(());
    };
    if store.meta_i64(META_CRED_MTIME_SEEN)? == Some(mtime) {
        return Ok(());
    }
    let Some(active) = store.active_account()? else {
        store.set_meta(META_CRED_MTIME_SEEN, mtime)?;
        return Ok(());
    };
    let Some(oauth) = credfile::read_oauth(&cred_path)? else {
        store.set_meta(META_CRED_MTIME_SEEN, mtime)?;
        return Ok(());
    };

    if credfile::is_dead(&oauth) {
        *dead_login = true;
        if store
            .last_event_of(kind::DEAD_LOGIN)?
            .is_none_or(|e| e.account_from != Some(active.id) || e.ts < mtime)
        {
            store.log_event(
                kind::DEAD_LOGIN,
                Some(active.id),
                None,
                Some("refresh_rejected"),
                None,
            )?;
        }
        // Leave the mtime unseen so every tick keeps requesting a swap until one lands.
        return Ok(());
    }

    let stored_fp = store.credential_meta(active.id)?.map(|m| m.refresh_fp);
    let file_fp = credfile::oauth_fingerprint(&oauth);
    if file_fp.is_some() && file_fp != stored_fp {
        store.save_credential(active.id, &oauth, "sync")?;
        store.log_event(kind::SYNC, Some(active.id), None, Some("refreshed"), None)?;
    }
    store.set_meta(META_CRED_MTIME_SEEN, mtime)?;
    Ok(())
}

/// If someone ran `/login` inside the rotation profile, `.claude.json` names a different
/// account. When that account is in the pool, make it active and store its new login; when it
/// is not, record the foreign login once and stop syncing until it changes back.
///
/// Returns true when the tick must not sync the credentials file this time.
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
            let oauth = credfile::read_oauth(&paths.credentials_file())?;
            store.immediate(|s| {
                if let Some(a) = &active {
                    s.set_state(a.id, AccountState::Parked)?;
                }
                s.set_state(account.id, AccountState::Active)?;
                if let Some(o) = oauth.as_ref().filter(|o| !credfile::is_dead(o)) {
                    s.save_credential(account.id, o, "sync")?;
                }
                s.log_event(
                    kind::ADOPTED_LOGIN,
                    active.as_ref().map(|a| a.id),
                    Some(account.id),
                    Some("login_in_profile"),
                    None,
                )?;
                Ok(())
            })?;
            if let Some(m) = credfile::mtime_ms(&paths.credentials_file()) {
                store.set_meta(META_CRED_MTIME_SEEN, m)?;
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
            r#"{"session_id":"s1","rate_limits":{"five_hour":{"used_percentage":41.5,"resets_at":1792890000},
                "seven_day":{"used_percentage":12,"resets_at":"2026-10-30T00:00:00Z"}}}"#,
        );
        let rl = input.rate_limits.unwrap();
        assert_eq!(rl.five_hour.as_ref().unwrap().used_percentage, Some(41.5));
        assert_eq!(rl.five_hour.unwrap().resets_at, Some(1_792_890_000));
        assert_eq!(rl.seven_day.unwrap().resets_at, Some(1_793_318_400));
    }

    #[test]
    fn tolerates_garbage() {
        assert!(StatusInput::parse("not json").rate_limits.is_none());
        assert!(StatusInput::parse("{}").rate_limits.is_none());
    }

    #[test]
    fn iso8601_offsets() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_iso8601("2000-03-01T00:00:00.123Z"), Some(951_868_800));
        assert_eq!(parse_iso8601("garbage"), None);
    }
}
