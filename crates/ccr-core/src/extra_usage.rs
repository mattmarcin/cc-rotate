//! Extra usage (spec section 22): what an account bills once its plan's limits are used up.
//!
//! cc-rotate swaps before a limit, so an account only spends extra usage once every account is at
//! a limit: then the account in use keeps working, and bills, while the others would stop and
//! wait. `scan` tells the user once per such episode; with `avoid_extra_usage` on, `avoid_swap`
//! moves the profile to an account without extra usage, so the sessions wait for a reset
//! instead.

use anyhow::Result;
use serde_json::json;

use crate::scheduler::{self, Candidate, Policy, Reason};
use crate::store::{Account, AccountState, ExtraUsage, Store, kind};
use crate::swap::{self, SwapRequest};

/// The account in use when the last `paid_usage` warning went out, until the episode ends.
const META_WARNED: &str = "paid_usage_warned";
/// When the current episode of paid extra usage began (epoch ms).
const META_PAYING_SINCE: &str = "paid_usage_since";
/// When a fresh reading of the paying account was last asked for (epoch ms).
const META_REFRESH_AT: &str = "paid_usage_refresh_at";

/// How long a swap away from paid extra usage has before the user is told anyway: it may never
/// land (a dead login, an interrupted swap, a busy target).
const AVOID_GRACE_MS: i64 = 2 * 60_000;
/// A paying account's extra usage is read again at most this often.
const REFRESH_EVERY_MS: i64 = 10 * 60_000;

/// A window this full is used up: requests beyond it bill extra usage. cc-rotate's thresholds
/// (98% by default, lower with a weekly cap) stop rotating onto an account before that.
const USED_UP_PCT: f64 = 99.5;

/// Whether the account in use runs on paid extra usage: no other account can be picked, a limit
/// that applies to it is used up, and its latest report says extra usage is on.
fn paying(
    store: &Store,
    pool: &[Candidate],
    policy: Policy,
    active: &Account,
    now_s: i64,
) -> Result<Option<(ExtraUsage, i64)>> {
    let at_limit = pool.iter().any(|c| {
        c.account.id == active.id && c.windows.iter().any(|w| w.applies && w.pct >= USED_UP_PCT)
    });
    if !at_limit || scheduler::pick(pool, policy, Some(active.id), now_s).is_some() {
        return Ok(None);
    }
    Ok(store.extra_usage(active.id)?.filter(|(e, _)| e.enabled))
}

/// Log `paid_usage` once when sessions start running on paid extra usage, which the app shows as
/// a notification. Another warning needs the episode to end first (an account free again, or the
/// one in use without extra usage). With `avoid_extra_usage` on and an account to move to, the
/// swap gets `AVOID_GRACE_MS` to end the episode first.
pub fn scan(store: &Store, now_ms: i64) -> Result<()> {
    let Some(active) = store.active_account()? else {
        return Ok(());
    };
    let (pool, policy) = swap::load_pool(store, now_ms / 1000)?;
    let paying = paying(store, &pool, policy, &active, now_ms / 1000)?;
    let since = match &paying {
        Some(_) => store.immediate(|s| match s.meta_i64(META_PAYING_SINCE)? {
            Some(t) => Ok(t),
            None => {
                s.set_meta(META_PAYING_SINCE, now_ms)?;
                Ok(now_ms)
            }
        })?,
        None => now_ms,
    };
    let can_move =
        paying.is_some() && avoid_swap(store, &pool, policy, &active, now_ms / 1000)?.is_some();
    match paying {
        Some(_) if can_move && now_ms - since < AVOID_GRACE_MS => Ok(()),
        Some((extra, read_at)) => store.immediate(|s| {
            if s.meta_i64(META_WARNED)? == Some(active.id) {
                return Ok(());
            }
            s.set_meta(META_WARNED, active.id)?;
            s.log_event(
                kind::PAID_USAGE,
                Some(active.id),
                None,
                None,
                Some(json!({
                    "label": active.label,
                    "used": extra.used,
                    "monthly_limit": extra.monthly_limit,
                    "currency": extra.currency,
                    // The account in use is rarely read: say how old the figures are.
                    "read_at": read_at,
                    // With the setting on, it still pays: no account without extra usage to go
                    // to, or the swap to one did not land.
                    "avoiding": s.settings()?.avoid_extra_usage,
                    "could_move": can_move,
                })),
            )?;
            Ok(())
        }),
        None => {
            for key in [META_WARNED, META_PAYING_SINCE] {
                if store.meta(key)?.is_some() {
                    store.delete_meta(key)?;
                }
            }
            Ok(())
        }
    }
}

/// Whether to read the account in use's usage again (`usage_check::confirm_active`): it runs on
/// paid extra usage by a report older than `usage_check_max_age_s`, and none was asked for in the
/// last `REFRESH_EVERY_MS`. The account in use is rarely read, and a new billing month or a
/// change on claude.ai may have turned its extra usage on or off since.
pub fn needs_fresh_reading(
    store: &Store,
    pool: &[Candidate],
    policy: Policy,
    active: &Account,
    now_ms: i64,
) -> Result<bool> {
    let settings = store.settings()?;
    if !settings.usage_checks {
        return Ok(false);
    }
    let Some((_, read_at)) = paying(store, pool, policy, active, now_ms / 1000)? else {
        return Ok(false);
    };
    if now_ms - read_at < settings.usage_check_max_age_s * 1000 {
        return Ok(false);
    }
    store.immediate(|s| {
        let due = s
            .meta_i64(META_REFRESH_AT)?
            .is_none_or(|t| !(0..REFRESH_EVERY_MS).contains(&(now_ms - t)));
        if due {
            s.set_meta(META_REFRESH_AT, now_ms)?;
        }
        Ok(due)
    })
}

/// With `avoid_extra_usage` on, a swap from an account running on paid extra usage to one whose
/// report says extra usage is off, the one that frees up first, so the sessions wait for a reset
/// instead of paying. Paused and signed-out accounts are left alone, and an account whose extra
/// usage is not known is not taken for one without.
pub fn avoid_swap(
    store: &Store,
    pool: &[Candidate],
    policy: Policy,
    active: &Account,
    now_s: i64,
) -> Result<Option<SwapRequest>> {
    if !store.settings()?.avoid_extra_usage || paying(store, pool, policy, active, now_s)?.is_none()
    {
        return Ok(None);
    }
    let mut unpaid = Vec::new();
    for c in pool {
        if c.account.id == active.id || c.account.state != AccountState::Parked || c.held_back {
            continue;
        }
        if store
            .extra_usage(c.account.id)?
            .is_some_and(|(extra, _)| !extra.enabled)
        {
            unpaid.push(c);
        }
    }
    Ok(unpaid
        .into_iter()
        .min_by_key(|c| (c.blocked_until().unwrap_or(i64::MAX), c.order))
        .map(|c| SwapRequest {
            from: Some(active.id),
            to: Some(c.account.id),
            reason: Reason::AvoidExtraUsage,
            wait: false,
        }))
}

/// An amount in minor units of `currency` (cents for USD): "$12.40", "12.40 EUR", "1200 JPY".
pub fn money(minor: i64, currency: Option<&str>) -> String {
    let currency = currency.unwrap_or("USD").to_ascii_uppercase();
    // ISO 4217 currencies without minor units that a plan may bill in.
    let whole = matches!(currency.as_str(), "JPY" | "KRW" | "CLP" | "ISK" | "VND");
    let amount = if whole {
        minor.to_string()
    } else {
        format!("{}.{:02}", minor.div_euclid(100), minor.rem_euclid(100))
    };
    match currency.as_str() {
        "USD" => format!("${amount}"),
        other => format!("{amount} {other}"),
    }
}

/// "on, $12.40 of $50.00 used this month", "on", "off, its $50.00 monthly limit is used up", or
/// "off".
pub fn describe(extra: &ExtraUsage) -> String {
    let currency = extra.currency.as_deref();
    if !extra.enabled {
        return match (extra.used, extra.monthly_limit) {
            (Some(used), Some(limit)) if limit > 0 && used >= limit => format!(
                "off, its {} monthly limit is used up",
                money(limit, currency)
            ),
            _ => "off".into(),
        };
    }
    match (extra.used, extra.monthly_limit) {
        (Some(used), Some(limit)) => format!(
            "on, {} of {} used this month",
            money(used, currency),
            money(limit, currency)
        ),
        (Some(used), None) => format!("on, {} used this month", money(used, currency)),
        _ => "on".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_money_and_states() {
        assert_eq!(money(1240, Some("USD")), "$12.40");
        assert_eq!(money(5, None), "$0.05");
        assert_eq!(money(1240, Some("eur")), "12.40 EUR");
        assert_eq!(money(1200, Some("JPY")), "1200 JPY");
        let mut extra = ExtraUsage {
            enabled: true,
            monthly_limit: Some(5000),
            used: Some(1240),
            utilization: Some(24.8),
            currency: Some("USD".into()),
            disabled_reason: None,
        };
        assert_eq!(describe(&extra), "on, $12.40 of $50.00 used this month");
        extra.monthly_limit = None;
        assert_eq!(describe(&extra), "on, $12.40 used this month");
        extra.enabled = false;
        assert_eq!(describe(&extra), "off");
        extra.monthly_limit = Some(1240);
        assert_eq!(describe(&extra), "off, its $12.40 monthly limit is used up");
    }
}
