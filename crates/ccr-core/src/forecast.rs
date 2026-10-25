//! How long the pool lasts at the current pace (spec section 7.5).
//!
//! The last hour of readings gives the pace, as a multiple of each account's usual pace (so it
//! carries across swaps and plans of different sizes). The pool is then played forward in short
//! steps the way rotation would run it: the account in use fills its windows, a limit hands over
//! to the account the scheduler would pick, and windows reset when they reset. The first moment
//! no account is left is when the pool runs out, and the earliest reset after it is when an
//! account frees up again.

use anyhow::Result;
use serde::Serialize;

use crate::budget::Rates;
use crate::scheduler::{self, Candidate, Policy, WindowKind};
use crate::store::{AccountState, Store};

/// How far ahead the pool is played.
pub const HORIZON_S: i64 = 24 * 3600;
const STEP_S: i64 = 300;
/// The pace is measured over the last hour of readings.
const PACE_WINDOW_MS: i64 = 60 * 60_000;
/// Readings spanning less than this do not show a pace yet.
const MIN_PACE_SPAN_MS: i64 = 15 * 60_000;
/// Two readings further apart than this do not show a pace.
const MAX_GAP_MS: i64 = 20 * 60_000;
const FIVE_HOUR_S: i64 = 5 * 3600;
const WEEK_S: i64 = 7 * 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Forecast {
    /// How hard the pool was used in the last hour, as a multiple of each account's usual pace
    /// (`budget::AccountRates::five_per_hour`): 1 is the usual pace.
    pub pace: f64,
    /// When every account would be at a limit (epoch seconds), if within `HORIZON_S`.
    pub runs_out_at: Option<i64>,
    /// When the first account frees up again after that (epoch seconds).
    pub free_again_at: Option<i64>,
}

/// How hard the pool was used in the last hour, as a multiple of each account's usual pace:
/// `None` while the readings show no use, or span too little time to tell.
pub fn pace(store: &Store, rates: &Rates, now_ms: i64) -> Result<Option<f64>> {
    let (mut first, mut last) = (i64::MAX, i64::MIN);
    let mut usual_hours = 0.0;
    for account in store.accounts()? {
        let samples = store.account_samples_since(account.id, now_ms - PACE_WINDOW_MS)?;
        if let (Some(a), Some(b)) = (samples.first(), samples.last()) {
            first = first.min(a.ts);
            last = last.max(b.ts);
        }
        let usual = rates.account(account.id).five_per_hour;
        for pair in samples.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            if b.ts - a.ts > MAX_GAP_MS {
                continue;
            }
            let five = |s: &crate::store::Sample| {
                s.windows
                    .iter()
                    .find(|w| w.name == scheduler::FIVE_HOUR)
                    .map(|w| (w.pct, w.resets_at))
            };
            if let (Some((pa, ra)), Some((pb, rb))) = (five(a), five(b)) {
                if ra == rb && pb > pa {
                    usual_hours += (pb - pa) / usual;
                }
            }
        }
    }
    let span = last.saturating_sub(first);
    if span < MIN_PACE_SPAN_MS || usual_hours <= 0.0 {
        return Ok(None);
    }
    Ok(Some(usual_hours / (span as f64 / 3_600_000.0)))
}

/// Play the pool forward from `now_s` at `pace` on the account in use.
pub fn forecast(
    pool: &[Candidate],
    policy: Policy,
    rates: &Rates,
    pace: f64,
    now_s: i64,
) -> Forecast {
    let mut pool: Vec<Candidate> = pool.to_vec();
    let mut current = pool
        .iter()
        .position(|c| c.account.state == AccountState::Active);
    let mut out = Forecast {
        pace,
        runs_out_at: None,
        free_again_at: None,
    };

    let mut t = now_s;
    while t < now_s + HORIZON_S {
        for c in &mut pool {
            for w in &mut c.windows {
                if let Some(r) = w.resets_at.filter(|&r| r <= t) {
                    w.pct = 0.0;
                    w.resets_at = match w.kind {
                        // A 5-hour window starts again with the next request.
                        WindowKind::Session => None,
                        _ => Some(r + WEEK_S),
                    };
                }
            }
        }
        if current.is_none_or(|i| pool[i].blocking().is_some()) {
            if let Some(i) = current {
                pool[i].account.state = AccountState::Parked;
            }
            current = scheduler::pick(&pool, policy, None)
                .map(|c| c.account.id)
                .and_then(|id| pool.iter().position(|c| c.account.id == id));
            match current {
                Some(i) => pool[i].account.state = AccountState::Active,
                None => {
                    out.runs_out_at = Some(t);
                    out.free_again_at = pool
                        .iter()
                        .filter(|c| c.usable() && !c.held_back)
                        .filter_map(|c| c.blocked_until())
                        .filter(|&r| r > t)
                        .min();
                    return out;
                }
            }
        }
        let i = current.expect("an account is in use here");
        let r = rates.account(pool[i].account.id);
        let five_used = pace * r.five_per_hour * STEP_S as f64 / 3600.0;
        for w in &mut pool[i].windows {
            match w.kind {
                WindowKind::Session => {
                    w.pct = (w.pct + five_used).min(100.0);
                    w.resets_at.get_or_insert(t + FIVE_HOUR_S);
                }
                WindowKind::Weekly => {
                    w.pct = (w.pct + five_used * r.weekly_per_5h).min(100.0);
                }
                WindowKind::Other => {}
            }
        }
        // An account without a 5-hour window in its readings gets one as it is used.
        if !pool[i]
            .windows
            .iter()
            .any(|w| w.kind == WindowKind::Session && w.model.is_none())
        {
            pool[i].windows.push(scheduler::WindowUsage {
                name: scheduler::FIVE_HOUR.into(),
                label: scheduler::window_label(scheduler::FIVE_HOUR),
                kind: WindowKind::Session,
                model: None,
                pct: five_used,
                resets_at: Some(t + FIVE_HOUR_S),
                threshold: policy.thresholds.session,
                applies: true,
            });
        }
        t += STEP_S;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::AccountRates;
    use crate::scheduler::{Thresholds, WeeklyLimit, window_usage};
    use crate::store::{Account, Sample, SampleWindow, WeeklyLimitSetting};

    const NOW: i64 = 1_800_000_000;
    const T: Thresholds = Thresholds {
        session: 98.0,
        weekly: 98.0,
    };
    const POLICY: Policy = Policy {
        thresholds: T,
        prefer_no_weekly_limit: true,
        use_expiring_weekly: false,
    };

    fn seat(id: i64, state: AccountState, five: f64, resets_in_s: Option<i64>) -> Candidate {
        let sample = Sample {
            ts: NOW * 1000,
            account_id: id,
            profile: "rotate".into(),
            session_id: None,
            model: None,
            windows: vec![SampleWindow {
                name: "five_hour".into(),
                pct: five,
                resets_at: resets_in_s.map(|s| NOW + s),
            }],
        };
        Candidate {
            account: Account {
                id,
                label: format!("s{id}"),
                email: None,
                account_uuid: None,
                org_uuid: None,
                org_name: None,
                subscription_type: None,
                rate_limit_tier: None,
                state,
                position: id,
                weekly_limit: WeeklyLimitSetting::Auto,
                weekly_cap: None,
                added_at: 0,
                last_login_at: None,
            },
            windows: window_usage(Some(&sample), NOW, T, None),
            weekly_detected: WeeklyLimit::None,
            weekly_limit: WeeklyLimit::None,
            order: id as usize,
            held_back: false,
            expiring: None,
        }
    }

    fn same_pace() -> Rates {
        let mut rates = Rates::default();
        for id in 1..=3 {
            rates.accounts.insert(
                id,
                AccountRates {
                    weekly_per_5h: 0.1,
                    five_per_hour: 20.0,
                },
            );
        }
        rates
    }

    #[test]
    fn a_pool_with_room_lasts_the_horizon() {
        // Half the usual 20 points an hour: a 5-hour window never fills.
        let pool = [
            seat(1, AccountState::Active, 0.0, None),
            seat(2, AccountState::Parked, 0.0, None),
        ];
        let f = forecast(&pool, POLICY, &same_pace(), 0.5, NOW);
        assert_eq!(f.runs_out_at, None);
    }

    #[test]
    fn seats_that_reset_faster_than_they_fill_never_run_out() {
        // 49 points an hour: a fresh window lasts 2 hours and resets 5 hours after it starts.
        // Three seats give 6 hours of use in every 5.
        let pool = [
            seat(1, AccountState::Active, 49.0, Some(3 * 3600)),
            seat(2, AccountState::Parked, 0.0, None),
            seat(3, AccountState::Parked, 0.0, None),
        ];
        let f = forecast(&pool, POLICY, &same_pace(), 49.0 / 20.0, NOW);
        assert_eq!(f.runs_out_at, None, "{:?}", f);
    }

    #[test]
    fn two_seats_at_a_pace_they_cannot_cover_run_out() {
        // 98 points an hour: each window lasts one hour, and two seats cover two of every five
        // hours.
        let pool = [
            seat(1, AccountState::Active, 0.0, None),
            seat(2, AccountState::Parked, 0.0, None),
        ];
        let f = forecast(&pool, POLICY, &same_pace(), 98.0 / 20.0, NOW);
        let out = f.runs_out_at.expect("runs out");
        assert!(
            (NOW + 3600..=NOW + 2 * 3600 + STEP_S).contains(&out),
            "{}",
            out - NOW
        );
        let free = f.free_again_at.expect("frees up");
        // s1's window started at once and resets 5 hours later.
        assert!((NOW + 5 * 3600 - STEP_S..=NOW + 5 * 3600 + STEP_S).contains(&free));
    }
}
