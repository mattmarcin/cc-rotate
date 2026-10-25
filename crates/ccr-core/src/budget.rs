//! Spending weekly budget before it resets (spec section 7.4).
//!
//! What an account with a weekly limit has left at its weekly reset is lost. Its budget can only
//! be spent as fast as its 5-hour windows allow, and only while the user is working, so an
//! account whose budget would otherwise reset unused moves ahead of the others early enough to
//! spend it. It moves ahead only then: until it has to, its budget stays in reserve for when
//! every other account is at a limit.
//!
//! Three rates, learned from the last `LEARN_DAYS` of readings, each starting from a cautious
//! guess that the readings outweigh as they come in:
//! - how many weekly points one 5-hour point costs on the account;
//! - how many 5-hour points an hour of use spends on the account;
//! - how likely the user is to be working in each hour of the week.

use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::scheduler::{Candidate, WeeklyLimit};
use crate::store::{AccountState, Sample, Store};

/// How far back the rates are learned from.
pub const LEARN_DAYS: i64 = 28;
/// The learned rates are relearned after this long (they move slowly).
const RATES_TTL_MS: i64 = 10 * 60_000;
const META_RATES: &str = "budget_rates";
/// The weekly reset (epoch seconds) of the window in which a swap took the account because its
/// budget would otherwise reset unused (`promoted_key`).
const META_PROMOTED_PREFIX: &str = "budget_promoted:";
/// Reset times this close are the same reset (the status line and `/usage` round differently).
const SAME_RESET_S: i64 = 60;

/// The guess before readings: a full 5-hour window costs a tenth of the weekly budget. On the
/// low side on purpose: a cheaper window means more hours are needed, so the account moves
/// ahead earlier rather than too late.
const PRIOR_WEEKLY_PER_5H: f64 = 0.10;
/// How many 5-hour points of readings weigh as much as the guess.
const PRIOR_5H_POINTS: f64 = 50.0;
/// The guess for how fast an hour of use spends the 5-hour window: half of the most it can.
const PRIOR_5H_PER_HOUR: f64 = 10.0;
const PRIOR_HOURS: f64 = 2.0;
/// The guess for the chance of working in any hour: 10 hours a day.
const PRIOR_ACTIVITY: f64 = 10.0 / 24.0;

/// Two readings further apart than this do not show a rate: the account may have sat idle.
const MAX_GAP_MS: i64 = 20 * 60_000;
/// Two changes of the 5-hour window further apart than this are not one stretch of use: the
/// sessions may have sat open and idle in between.
const MAX_STRETCH_MS: i64 = 30 * 60_000;
/// Less weekly budget left than this is not worth moving an account ahead for.
pub const MIN_LEFT: f64 = 5.0;
/// An account moves ahead once spending its budget, and that of every account resetting
/// before it, would take this share of the working hours left before its reset.
pub const ENTER: f64 = 0.75;
/// An account that moved ahead this week (`promoted`) stays ahead while the share is at least
/// this, so it is not given up as its own spending lowers the share, and is taken back after
/// its 5-hour limit.
pub const STAY: f64 = 0.3;
const HOURS_PER_WEEK: usize = 168;

/// What one account's readings say about it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AccountRates {
    /// Weekly points one 5-hour point costs.
    pub weekly_per_5h: f64,
    /// 5-hour points an hour of use spends.
    pub five_per_hour: f64,
}

impl Default for AccountRates {
    fn default() -> Self {
        Self {
            weekly_per_5h: PRIOR_WEEKLY_PER_5H,
            five_per_hour: PRIOR_5H_PER_HOUR,
        }
    }
}

/// The learned rates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rates {
    /// Epoch ms.
    pub computed_at: i64,
    pub accounts: HashMap<i64, AccountRates>,
    /// The chance the user is working in each hour of the week, indexed by
    /// `epoch hours % 168`.
    pub activity: Vec<f64>,
}

impl Default for Rates {
    fn default() -> Self {
        Self {
            computed_at: 0,
            accounts: HashMap::new(),
            activity: vec![PRIOR_ACTIVITY; HOURS_PER_WEEK],
        }
    }
}

impl Rates {
    pub fn account(&self, id: i64) -> AccountRates {
        self.accounts.get(&id).copied().unwrap_or_default()
    }

    /// The working hours the user is expected to have between two moments (epoch seconds).
    pub fn working_hours(&self, from_s: i64, until_s: i64) -> f64 {
        let mut hours = 0.0;
        let mut t = from_s;
        while t < until_s {
            let next = (t / 3600 + 1) * 3600;
            let end = next.min(until_s);
            let bucket = (t / 3600).rem_euclid(HOURS_PER_WEEK as i64) as usize;
            hours += (end - t) as f64 / 3600.0 * self.activity[bucket];
            t = end;
        }
        hours
    }
}

/// The last learned rates, whatever their age, or the guesses before any were learned. Only
/// reads: the status line and the snapshot use this; `refresh_rates` relearns them.
pub fn cached_rates(store: &Store) -> Result<Rates> {
    Ok(store
        .meta(META_RATES)?
        .and_then(|v| serde_json::from_str::<Rates>(&v).ok())
        .filter(|r| r.activity.len() == HOURS_PER_WEEK)
        .unwrap_or_default())
}

/// The learned rates, relearned from the readings when the stored ones are older than
/// `RATES_TTL_MS` (or from a clock that has since been set back). It reads weeks of readings,
/// so it runs off the status line: before a swap, in the app, and in `status` and `next`.
pub fn refresh_rates(store: &Store, now_ms: i64) -> Result<Rates> {
    if let Some(cached) = store
        .meta(META_RATES)?
        .and_then(|v| serde_json::from_str::<Rates>(&v).ok())
        .filter(|r| {
            (0..RATES_TTL_MS).contains(&(now_ms - r.computed_at))
                && r.activity.len() == HOURS_PER_WEEK
        })
    {
        return Ok(cached);
    }
    let since = now_ms - LEARN_DAYS * 86_400_000;
    let mut by_account = Vec::new();
    for account in store.accounts()? {
        by_account.push((account.id, store.account_samples_since(account.id, since)?));
    }
    let rates = learn(&by_account, now_ms);
    store.set_meta(META_RATES, serde_json::to_string(&rates)?)?;
    Ok(rates)
}

/// The meta key recording that a swap took the account for its expiring weekly budget.
pub fn promoted_key(account_id: i64) -> String {
    format!("{META_PROMOTED_PREFIX}{account_id}")
}

/// Mark the account in use as moved ahead for this weekly window once its budget would reset
/// unused (it got there while in use, not by a swap that marked it). Called on status line
/// ticks and the app's syncs, which write anyway.
pub fn note_in_use(store: &Store, pool: &[Candidate]) -> Result<()> {
    for c in pool {
        if c.account.state != AccountState::Active {
            continue;
        }
        if let Some(e) = &c.expiring {
            let key = promoted_key(c.account.id);
            if store.meta_i64(&key)? != Some(e.resets_at) {
                store.set_meta(&key, e.resets_at)?;
            }
        }
    }
    Ok(())
}

/// The weekly reset of the window in which each account was taken for its expiring budget.
pub fn promoted(store: &Store, pool: &[Candidate]) -> Result<HashMap<i64, i64>> {
    let mut out = HashMap::new();
    for c in pool.iter().filter(|c| c.weekly_limit == WeeklyLimit::Has) {
        if let Some(reset) = store.meta_i64(&promoted_key(c.account.id))? {
            out.insert(c.account.id, reset);
        }
    }
    Ok(out)
}

/// Learn the rates from each account's readings, oldest first.
///
/// Readings report whole or half points and repeat between changes, so rates are measured
/// over stretches from one rise of the 5-hour window to the next: the 5-hour points, the
/// weekly points and the time between two rises. Several sessions report the same account, and
/// an idle one keeps repeating its last values, so a reading below the highest one of its
/// window, or of an older window, is a stale session's and is passed over. A stretch longer
/// than `MAX_STRETCH_MS`, a gap between readings longer than `MAX_GAP_MS` or a newer window
/// starts afresh.
pub fn learn(by_account: &[(i64, Vec<Sample>)], now_ms: i64) -> Rates {
    let mut rates = Rates {
        computed_at: now_ms,
        ..Rates::default()
    };
    let mut active_hours: Vec<i64> = Vec::new();
    let mut first_ms = now_ms;
    for (id, samples) in by_account {
        let (mut five, mut weekly_five, mut weekly, mut hours) = (0.0, 0.0, 0.0, 0.0);
        let mut anchor: Option<&Sample> = None;
        let mut prev: Option<&Sample> = None;
        for s in samples.iter().filter(|s| s.ts <= now_ms) {
            first_ms = first_ms.min(s.ts);
            let Some(now_five) = window(s, "five_hour") else {
                continue;
            };
            let restart = match (anchor, prev) {
                (Some(a), Some(p)) => {
                    let then = window(a, "five_hour").expect("an anchor has a 5-hour window");
                    if older(now_five.resets_at, then.resets_at)
                        || (same_reset(then.resets_at, now_five.resets_at)
                            && now_five.pct < then.pct)
                    {
                        // A stale session's reading: it shows nothing new.
                        prev = Some(s);
                        continue;
                    }
                    s.ts - p.ts > MAX_GAP_MS || !same_reset(then.resets_at, now_five.resets_at)
                }
                _ => true,
            };
            prev = Some(s);
            if restart {
                anchor = Some(s);
                continue;
            }
            let a = anchor.expect("not restarting means an anchor");
            let then = window(a, "five_hour").expect("an anchor has a 5-hour window");
            let d5 = now_five.pct - then.pct;
            if d5 <= 0.0 {
                continue;
            }
            let span = s.ts - a.ts;
            if span > 0 && span <= MAX_STRETCH_MS {
                five += d5;
                hours += span as f64 / 3_600_000.0;
                active_hours.extend(a.ts / 3_600_000..=s.ts / 3_600_000);
                if let (Some(w0), Some(w1)) = (window(a, "seven_day"), window(s, "seven_day")) {
                    if same_reset(w0.resets_at, w1.resets_at) && w1.pct >= w0.pct {
                        weekly_five += d5;
                        weekly += w1.pct - w0.pct;
                    }
                }
            }
            anchor = Some(s);
        }
        rates.accounts.insert(
            *id,
            AccountRates {
                weekly_per_5h: (weekly + PRIOR_WEEKLY_PER_5H * PRIOR_5H_POINTS)
                    / (weekly_five + PRIOR_5H_POINTS),
                five_per_hour: (five + PRIOR_5H_PER_HOUR * PRIOR_HOURS) / (hours + PRIOR_HOURS),
            },
        );
    }
    // Each hour of the week: in how many of the weeks seen was the user working then.
    active_hours.sort_unstable();
    active_hours.dedup();
    let mut worked = [0.0f64; HOURS_PER_WEEK];
    for h in &active_hours {
        worked[h.rem_euclid(HOURS_PER_WEEK as i64) as usize] += 1.0;
    }
    let (first_h, now_h) = (first_ms / 3_600_000, now_ms / 3_600_000);
    let mut seen = [0.0f64; HOURS_PER_WEEK];
    for h in first_h..=now_h {
        seen[h.rem_euclid(HOURS_PER_WEEK as i64) as usize] += 1.0;
    }
    for b in 0..HOURS_PER_WEEK {
        rates.activity[b] = ((worked[b] + PRIOR_ACTIVITY) / (seen[b] + 1.0)).min(1.0);
    }
    rates
}

fn window<'a>(s: &'a Sample, name: &str) -> Option<&'a crate::store::SampleWindow> {
    s.windows.iter().find(|w| w.name == name)
}

/// Whether window reset `a` is of a window before `b` (an older 5-hour window).
fn older(a: Option<i64>, b: Option<i64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a < b - SAME_RESET_S,
        (None, Some(_)) => true,
        _ => false,
    }
}

fn same_reset(a: Option<i64>, b: Option<i64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() <= SAME_RESET_S,
        (None, None) => true,
        _ => false,
    }
}

/// An account whose weekly budget would reset unused unless it is used now.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Expiring {
    /// Weekly points left before the account's weekly threshold (its cap, if lower).
    pub left: f64,
    /// Hours of use spending them takes, at the account's pace and within its 5-hour limit.
    pub hours_needed: f64,
    /// The working hours expected before the weekly reset.
    pub hours_available: f64,
    /// Epoch seconds.
    pub resets_at: i64,
}

/// The accounts that have to be used now to spend their weekly budget before it resets, by
/// account id.
///
/// Only one account is in use at a time, so the accounts are taken in the order their weekly
/// windows reset: an account must be used now when its hours, together with the hours of
/// every account resetting before it, take `ENTER` of the working hours it has left before its
/// reset. An account the user excluded (`Account::use_expiring` off) is never moved ahead,
/// and its hours do not count, since its budget is not planned to be spent early.
/// `STAY` applies instead to an account already moved ahead in this weekly window
/// (`promoted`, weekly resets by account id): taken by a swap for its budget, or found expiring
/// while in use (`note_in_use`). An account in use for other reasons (every seat was at a
/// limit) is not kept: its budget stays in reserve. An account blocked by another
/// window has only the working hours after that window resets; one blocked until its weekly
/// reset, or on hold, cannot spend anything and does not count.
pub fn expiring(
    pool: &[Candidate],
    rates: &Rates,
    promoted: &HashMap<i64, i64>,
    session_threshold: f64,
    now_s: i64,
) -> HashMap<i64, Expiring> {
    let mut spendable: Vec<(&Candidate, f64, f64, i64, i64)> = pool
        .iter()
        .filter(|c| {
            c.weekly_limit == WeeklyLimit::Has
                && c.account.use_expiring
                && matches!(c.account.state, AccountState::Parked | AccountState::Active)
                && !c.held_back
        })
        .filter_map(|c| {
            let week = c.weekly()?;
            let resets_at = week.resets_at.filter(|&r| r > now_s)?;
            let left = (week.threshold - week.pct).max(0.0);
            if left < MIN_LEFT {
                return None;
            }
            let from = c.blocked_until().filter(|&u| u > now_s).unwrap_or(now_s);
            if from >= resets_at {
                return None;
            }
            let r = rates.account(c.account.id);
            // The most a 5-hour window lets through, spread over its five hours.
            let five_per_hour = r.five_per_hour.min(session_threshold / 5.0);
            let per_hour = r.weekly_per_5h * five_per_hour;
            (per_hour > 0.0).then(|| (c, left, left / per_hour, from, resets_at))
        })
        .collect();
    spendable.sort_by_key(|&(c, _, _, _, resets_at)| (resets_at, c.account.id));

    let mut out = HashMap::new();
    let mut needed = 0.0;
    for (c, left, hours, from, resets_at) in spendable {
        needed += hours;
        let available = rates.working_hours(from, resets_at);
        if available <= 0.0 {
            continue;
        }
        let share = needed / available;
        let taken = promoted
            .get(&c.account.id)
            .is_some_and(|&r| same_reset(Some(r), Some(resets_at)));
        let bar = if taken { STAY } else { ENTER };
        if share >= bar {
            out.insert(
                c.account.id,
                Expiring {
                    left,
                    hours_needed: hours,
                    hours_available: available,
                    resets_at,
                },
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{Thresholds, window_usage};
    use crate::store::{Account, SampleWindow, WeeklyLimitSetting};

    const NOW: i64 = 1_800_000_000;
    const T: Thresholds = Thresholds {
        session: 98.0,
        weekly: 98.0,
        weekly_cap: None,
    };

    fn account(id: i64, state: AccountState) -> Account {
        Account {
            id,
            label: format!("a{id}"),
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
        }
    }

    fn weekly(id: i64, state: AccountState, used: f64, resets_in_h: i64) -> Candidate {
        let sample = Sample {
            ts: NOW * 1000,
            account_id: id,
            profile: "rotate".into(),
            session_id: None,
            model: None,
            windows: vec![
                SampleWindow {
                    name: "five_hour".into(),
                    pct: 0.0,
                    resets_at: None,
                },
                SampleWindow {
                    name: "seven_day".into(),
                    pct: used,
                    resets_at: Some(NOW + resets_in_h * 3600),
                },
            ],
        };
        Candidate {
            account: account(id, state),
            windows: window_usage(Some(&sample), NOW, T, None),
            weekly_detected: WeeklyLimit::Has,
            weekly_limit: WeeklyLimit::Has,
            order: id as usize,
            held_back: false,
            expiring: None,
        }
    }

    fn none() -> HashMap<i64, i64> {
        HashMap::new()
    }

    /// Working every hour, 1 weekly point per hour of use (10 five-hour points an hour at 0.1).
    fn always_working() -> Rates {
        Rates {
            activity: vec![1.0; HOURS_PER_WEEK],
            ..Rates::default()
        }
    }

    #[test]
    fn a_budget_that_fits_easily_stays_in_reserve() {
        // 38 points left at 1 an hour need 38 hours; 100 hours remain.
        let pool = [weekly(1, AccountState::Parked, 60.0, 100)];
        assert!(expiring(&pool, &always_working(), &none(), 98.0, NOW).is_empty());
    }

    #[test]
    fn a_budget_that_needs_most_of_the_time_left_moves_ahead() {
        // 78 points need 78 of the 100 hours left.
        let pool = [weekly(1, AccountState::Parked, 20.0, 100)];
        let out = expiring(&pool, &always_working(), &none(), 98.0, NOW);
        let e = out[&1];
        assert!((e.hours_needed - 78.0).abs() < 1e-6, "{e:?}");
        assert!((e.hours_available - 100.0).abs() < 1e-6, "{e:?}");
    }

    #[test]
    fn nights_off_count_against_the_time_left() {
        // 38 hours of use; 100 hours to the reset, but only 10 working hours a day: about 42.
        let pool = [weekly(1, AccountState::Parked, 60.0, 100)];
        let rates = Rates {
            activity: vec![PRIOR_ACTIVITY; HOURS_PER_WEEK],
            ..Rates::default()
        };
        assert!(expiring(&pool, &rates, &none(), 98.0, NOW).contains_key(&1));
    }

    #[test]
    fn accounts_resetting_earlier_share_the_time() {
        // Each needs 48 hours alone (fine at 100 and 120 hours), but only one account is in
        // use at a time: the later one needs 96 of its 120.
        let pool = [
            weekly(1, AccountState::Parked, 50.0, 100),
            weekly(2, AccountState::Parked, 50.0, 120),
        ];
        let out = expiring(&pool, &always_working(), &none(), 98.0, NOW);
        assert!(!out.contains_key(&1));
        assert!(out.contains_key(&2));
    }

    #[test]
    fn only_an_account_moved_ahead_stays_ahead_while_it_spends() {
        // 40 of 100 hours: below the bar to move ahead, above the bar to stay.
        let parked = [weekly(1, AccountState::Parked, 58.0, 100)];
        let active = [weekly(1, AccountState::Active, 58.0, 100)];
        assert!(expiring(&parked, &always_working(), &none(), 98.0, NOW).is_empty());
        // In use because the seats were at a limit: its budget fits, so the seats take over
        // again when they can.
        assert!(expiring(&active, &always_working(), &none(), 98.0, NOW).is_empty());
        let taken = HashMap::from([(1, NOW + 100 * 3600)]);
        assert!(expiring(&active, &always_working(), &taken, 98.0, NOW).contains_key(&1));
    }

    #[test]
    fn little_left_or_no_weekly_limit_is_never_moved_ahead() {
        let mut unlimited = weekly(2, AccountState::Parked, 0.0, 10);
        unlimited.weekly_limit = WeeklyLimit::None;
        let pool = [weekly(1, AccountState::Parked, 95.0, 1), unlimited];
        assert!(expiring(&pool, &always_working(), &none(), 98.0, NOW).is_empty());
    }

    #[test]
    fn an_excluded_account_is_never_moved_ahead_and_takes_no_time() {
        // Alone, Y needs 48 of its 124 hours; X (resetting first) would push it ahead, but X is
        // excluded, so neither moves ahead.
        let mut x = weekly(1, AccountState::Parked, 20.0, 100);
        x.account.use_expiring = false;
        let y = weekly(2, AccountState::Parked, 50.0, 124);
        let out = expiring(&[x, y], &always_working(), &none(), 98.0, NOW);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn a_paused_account_is_left_alone() {
        let pool = [weekly(1, AccountState::Paused, 0.0, 50)];
        assert!(expiring(&pool, &always_working(), &none(), 98.0, NOW).is_empty());
    }

    #[test]
    fn an_account_taken_for_its_budget_is_taken_back_after_its_5_hour_limit() {
        // 40 of 100 hours: parked, below the bar to move ahead; taken by a swap this week, it
        // keeps the lower bar.
        let pool = [weekly(1, AccountState::Parked, 58.0, 100)];
        let reset = NOW + 100 * 3600;
        let taken = HashMap::from([(1, reset + 30)]);
        assert!(expiring(&pool, &always_working(), &taken, 98.0, NOW).contains_key(&1));
        // Taken in an earlier week: it does not count.
        let earlier = HashMap::from([(1, reset - 7 * 86_400)]);
        assert!(expiring(&pool, &always_working(), &earlier, 98.0, NOW).is_empty());
    }

    #[test]
    fn an_account_that_cannot_spend_before_its_reset_does_not_count() {
        // X is blocked until its weekly reset (another window at its limit): it cannot spend,
        // and must not push Y, which alone needs 48 of its 124 hours, ahead.
        let mut x = weekly(1, AccountState::Parked, 20.0, 100);
        x.windows.push(crate::scheduler::WindowUsage {
            name: "seven_day_opus".into(),
            label: "Opus weekly".into(),
            kind: crate::scheduler::WindowKind::Weekly,
            model: Some("opus".into()),
            pct: 100.0,
            resets_at: Some(NOW + 100 * 3600),
            threshold: 98.0,
            applies: true,
        });
        let y = weekly(2, AccountState::Parked, 50.0, 124);
        let out = expiring(&[x, y], &always_working(), &none(), 98.0, NOW);
        assert!(out.is_empty(), "{out:?}");

        let mut held = weekly(1, AccountState::Parked, 20.0, 100);
        held.held_back = true;
        let y = weekly(2, AccountState::Parked, 50.0, 124);
        assert!(expiring(&[held, y], &always_working(), &none(), 98.0, NOW).is_empty());
    }

    fn reading(ts_min: i64, five: f64, five_reset: i64, week: Option<(f64, i64)>) -> Sample {
        let mut windows = vec![SampleWindow {
            name: "five_hour".into(),
            pct: five,
            resets_at: Some(five_reset),
        }];
        if let Some((pct, reset)) = week {
            windows.push(SampleWindow {
                name: "seven_day".into(),
                pct,
                resets_at: Some(reset),
            });
        }
        Sample {
            ts: (NOW + ts_min * 60) * 1000,
            account_id: 1,
            profile: "rotate".into(),
            session_id: None,
            model: None,
            windows,
        }
    }

    #[test]
    fn rates_come_from_readings_in_the_same_windows() {
        // 6 hours of readings every 10 minutes, 2 five-hour points each (12 an hour), and a
        // weekly point every 10 five-hour points (0.1 is the guess, 0.2 in the readings).
        let mut samples = Vec::new();
        for i in 0..36 {
            let five = 2.0 * i as f64;
            let week = (20.0 + 0.4 * i as f64, NOW + 86_400);
            samples.push(reading(i * 10, five, NOW + 6 * 3600, Some(week)));
        }
        let rates = learn(&[(1, samples)], (NOW + 6 * 3600) * 1000);
        let r = rates.account(1);
        // 70 five-hour points and 14 weekly points, with the guess (50 at 0.1) mixed in.
        assert!(
            (r.weekly_per_5h - (14.0 + 5.0) / 120.0).abs() < 1e-9,
            "{r:?}"
        );
        // 70 points over 35 ten-minute gaps, with 2 guessed hours at 10.
        let hours = 35.0 / 6.0;
        assert!(
            (r.five_per_hour - (70.0 + 20.0) / (hours + 2.0)).abs() < 1e-9,
            "{r:?}"
        );
        // The working hours were seen once each: half way between the guess and certain.
        let worked = (NOW / 3600 + 1).rem_euclid(168) as usize;
        assert!(rates.activity[worked] > PRIOR_ACTIVITY);
    }

    /// Readings in whole points, a minute apart, repeating between changes: 15 five-hour points
    /// an hour (a change every 4 minutes) and a weekly point every 10 of them. Pairs of
    /// readings mostly show no change; the rates must still come out right.
    #[test]
    fn coarse_readings_give_the_true_rates() {
        let mut samples = Vec::new();
        for minute in 0..=240i64 {
            let five = (minute / 4) as f64;
            let week = (10.0 + ((minute / 4) / 10) as f64, NOW + 86_400);
            samples.push(reading(minute, five, NOW + 5 * 3600, Some(week)));
        }
        let rates = learn(&[(1, samples)], (NOW + 4 * 3600) * 1000);
        let r = rates.account(1);
        // 60 five-hour points in 4 hours, with the 2 guessed hours at 10: (60 + 20) / 6.
        assert!((r.five_per_hour - 80.0 / 6.0).abs() < 0.01, "{r:?}");
        // 6 weekly points for 60 five-hour points: 0.1, which is also the guess.
        assert!((r.weekly_per_5h - 0.1).abs() < 0.005, "{r:?}");
    }

    /// Two sessions on one account, each ticking every 30 seconds: one working (30% to 33% over
    /// 40 minutes, 4.5 points an hour) and one idle, repeating 30%. The idle session's readings
    /// must not count its drops and the working one's rebounds as use.
    #[test]
    fn an_idle_session_repeating_old_values_is_no_use() {
        let mut samples = Vec::new();
        let reset = NOW + 4 * 3600;
        for half_minute in 0..=80i64 {
            let ts_min = half_minute / 2;
            let working = 30.0 + (3 * half_minute / 80) as f64;
            let mut fresh = reading(ts_min, working, reset, Some((10.0, NOW + 86_400)));
            fresh.ts += (half_minute % 2) * 30_000;
            let mut stale = reading(ts_min, 30.0, reset, Some((10.0, NOW + 86_400)));
            stale.ts = fresh.ts + 1_000;
            samples.push(fresh);
            samples.push(stale);
        }
        let r = learn(&[(1, samples)], (NOW + 41 * 60) * 1000).account(1);
        // 3 points in 40 minutes, with the 2 guessed hours at 10: (3 + 20) / (2/3 + 2).
        assert!(
            (r.five_per_hour - 23.0 / (2.0 / 3.0 + 2.0)).abs() < 0.5,
            "{r:?}"
        );
    }

    #[test]
    fn readings_across_a_reset_or_a_long_gap_are_not_a_rate() {
        let samples = vec![
            reading(0, 50.0, NOW + 3600, Some((10.0, NOW + 86_400))),
            // A new 5-hour window.
            reading(10, 5.0, NOW + 20_000, Some((12.0, NOW + 86_400))),
            // An hour later: the account may have idled.
            reading(70, 30.0, NOW + 20_000, Some((15.0, NOW + 86_400))),
        ];
        let r = learn(&[(1, samples)], (NOW + 7200) * 1000).account(1);
        assert_eq!(r, AccountRates::default());
    }

    #[test]
    fn the_chance_of_working_never_goes_above_certain() {
        let samples = vec![
            reading(0, 1.0, NOW + 3600, None),
            reading(5, 3.0, NOW + 3600, None),
            reading(10, 5.0, NOW + 3600, None),
        ];
        let rates = learn(&[(1, samples)], (NOW + 11 * 60) * 1000);
        assert!(
            rates.activity.iter().all(|&p| p <= 1.0),
            "{:?}",
            rates.activity
        );
    }

    #[test]
    fn working_hours_count_parts_of_hours() {
        let rates = always_working();
        assert!((rates.working_hours(NOW, NOW + 5400) - 1.5).abs() < 1e-9);
        assert_eq!(rates.working_hours(NOW, NOW), 0.0);
    }
}
