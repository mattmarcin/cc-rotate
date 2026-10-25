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
/// Readings this far back give each 5-hour window's highest value before the hour, so the first
/// fresh reading of the hour is not counted from a stale session's older value.
const SEED_MS: i64 = 6 * 3_600_000;
/// Use that started less than this long ago does not show a pace yet.
const MIN_PACE_SPAN_MS: i64 = 15 * 60_000;
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
/// `None` while the readings show no use, or the use started less than `MIN_PACE_SPAN_MS` ago.
///
/// Several sessions report the same account and an idle one keeps repeating its last values,
/// so only rises above the highest reading of each 5-hour window count, that highest reading
/// known from before the hour too. Work is taken to have started the typical gap between rises
/// before the first rise of the hour, and the pace is the use of the rises over the time from
/// then to now: open idle sessions, background checks or other profiles' readings do not
/// stretch it, and the pace fades once nothing is used any more.
pub fn pace(store: &Store, rates: &Rates, now_ms: i64) -> Result<Option<f64>> {
    // Every rise of the hour: when, and how many usual hours of use it shows.
    let mut rises: Vec<(i64, f64)> = Vec::new();
    for account in store.accounts()? {
        let usual = rates.account(account.id).five_per_hour;
        // The highest reading of the current window so far: (reset, percent).
        let mut high: Option<(Option<i64>, f64)> = None;
        for s in store.account_samples_since(account.id, now_ms - SEED_MS)? {
            if s.ts > now_ms {
                continue;
            }
            let Some(w) = s.windows.iter().find(|w| w.name == scheduler::FIVE_HOUR) else {
                continue;
            };
            match high {
                Some((reset, pct)) if same_reset(reset, w.resets_at) => {
                    if w.pct > pct {
                        if s.ts >= now_ms - PACE_WINDOW_MS {
                            rises.push((s.ts, (w.pct - pct) / usual));
                        }
                        high = Some((reset, w.pct));
                    }
                }
                // An older window: a stale session's reading.
                Some((reset, _)) if older(w.resets_at, reset) => {}
                _ => high = Some((w.resets_at, w.pct)),
            }
        }
    }
    // What the first rise shows built up before it, while the readings repeated the value
    // before (which an idle session does too): work is taken to have started the typical gap
    // between rises before it. One rise alone shows no pace.
    rises.sort_by_key(|&(ts, _)| ts);
    if rises.len() < 2 {
        return Ok(None);
    }
    let mut gaps: Vec<i64> = rises.windows(2).map(|r| r[1].0 - r[0].0).collect();
    gaps.sort_unstable();
    let typical = gaps[gaps.len() / 2];
    let start = (rises[0].0 - typical).max(now_ms - PACE_WINDOW_MS);
    let span = now_ms - start;
    let used: f64 = rises.iter().map(|&(_, hours)| hours).sum();
    if used <= 0.0 || span < MIN_PACE_SPAN_MS {
        return Ok(None);
    }
    Ok(Some(used / (span as f64 / 3_600_000.0)))
}

/// Reset times this close are the same reset (the status line and `/usage` round differently).
fn same_reset(a: Option<i64>, b: Option<i64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() <= 60,
        (None, None) => true,
        _ => false,
    }
}

/// Whether window reset `a` is of a window before `b`.
fn older(a: Option<i64>, b: Option<i64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a < b - 60,
        (None, Some(_)) => true,
        _ => false,
    }
}

/// Start a 5-hour window at `t`, with `pct` of it used: the reading's window, or a new one when
/// the readings have none.
fn start_window(c: &mut Candidate, t: i64, pct: f64, policy: Policy) {
    match c
        .windows
        .iter_mut()
        .find(|w| w.kind == WindowKind::Session && w.model.is_none())
    {
        Some(w) => {
            w.pct = pct;
            w.resets_at = Some(t + FIVE_HOUR_S);
        }
        None => c.windows.push(scheduler::WindowUsage {
            name: scheduler::FIVE_HOUR.into(),
            label: scheduler::window_label(scheduler::FIVE_HOUR),
            kind: WindowKind::Session,
            model: None,
            pct,
            resets_at: Some(t + FIVE_HOUR_S),
            threshold: policy.thresholds.for_account(&c.account).session,
            applies: true,
        }),
    }
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
            // An account the user keeps running gets its next window as soon as one ends, while
            // it is not in use (section 23).
            let kept = c.account.keep_running
                && c.account.state == AccountState::Parked
                && !c.held_back
                // Another limit keeps it out of use, and warm-ups skip it.
                && c.windows.iter().all(|w| {
                    !w.blocking() || (w.kind == WindowKind::Session && w.model.is_none())
                });
            for w in &mut c.windows {
                if let Some(r) = w.resets_at.filter(|&r| r <= t) {
                    w.pct = 0.0;
                    w.resets_at = match w.kind {
                        WindowKind::Session if kept && w.model.is_none() => Some(t + FIVE_HOUR_S),
                        // A 5-hour window starts again with the next request.
                        WindowKind::Session => None,
                        _ => Some(r + WEEK_S),
                    };
                }
            }
            if kept && !c.session_running(t) {
                start_window(c, t, 0.0, policy);
            }
        }
        if current.is_none_or(|i| pool[i].blocking().is_some()) {
            if let Some(i) = current {
                pool[i].account.state = AccountState::Parked;
            }
            current = scheduler::pick(&pool, policy, None, t)
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
        if pool[i].five_hour().is_none() {
            start_window(&mut pool[i], t, five_used, policy);
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
        weekly_cap: None,
    };
    const POLICY: Policy = Policy {
        thresholds: T,
        prefer_no_weekly_limit: true,
        use_expiring_weekly: false,
        strict_order: false,
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
                use_expiring: true,
                keep_running: false,
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
    fn a_seat_kept_running_starts_its_window_before_it_is_used() {
        // s1 is nearly full and resets in 6 hours; s2 takes over a step later and fills within
        // the hour, so s2's reset is the first one after the pool runs out.
        let pool = |keep: bool| {
            let mut s2 = seat(2, AccountState::Parked, 0.0, None);
            s2.account.keep_running = keep;
            [seat(1, AccountState::Active, 95.0, Some(6 * 3600)), s2]
        };
        let pace = 98.0 / 20.0;
        let used = forecast(&pool(false), POLICY, &same_pace(), pace, NOW);
        let kept = forecast(&pool(true), POLICY, &same_pace(), pace, NOW);
        assert!(used.runs_out_at.is_some() && kept.runs_out_at.is_some());
        // Kept running, s2's window started at once instead of when s2 was first used.
        assert_eq!(kept.free_again_at, Some(NOW + 5 * 3600));
        assert_eq!(used.free_again_at, Some(NOW + STEP_S + 5 * 3600));
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
