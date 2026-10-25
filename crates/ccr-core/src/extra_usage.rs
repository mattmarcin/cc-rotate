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

/// Whether the account in use runs on paid extra usage: every account is at a limit (no other
/// can be picked, and it is at one itself), and its latest report says extra usage is on.
fn paying(
    store: &Store,
    pool: &[Candidate],
    policy: Policy,
    active: &Account,
    now_s: i64,
) -> Result<Option<ExtraUsage>> {
    let at_limit = pool
        .iter()
        .any(|c| c.account.id == active.id && c.blocking().is_some());
    if !at_limit || scheduler::pick(pool, policy, Some(active.id), now_s).is_some() {
        return Ok(None);
    }
    Ok(store
        .extra_usage(active.id)?
        .map(|(extra, _)| extra)
        .filter(|e| e.enabled))
}

/// Log `paid_usage` once when sessions start running on paid extra usage, which the app shows as
/// a notification. Another warning needs the episode to end first (an account free again, or the
/// one in use without extra usage).
pub fn scan(store: &Store, now_ms: i64) -> Result<()> {
    let Some(active) = store.active_account()? else {
        return Ok(());
    };
    let (pool, policy) = swap::load_pool(store, now_ms / 1000)?;
    match paying(store, &pool, policy, &active, now_ms / 1000)? {
        Some(extra) => store.immediate(|s| {
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
                    // With the setting on, it still pays: no account without extra usage to go to.
                    "avoiding": s.settings()?.avoid_extra_usage,
                })),
            )?;
            Ok(())
        }),
        None => {
            if store.meta(META_WARNED)?.is_some() {
                store.delete_meta(META_WARNED)?;
            }
            Ok(())
        }
    }
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
