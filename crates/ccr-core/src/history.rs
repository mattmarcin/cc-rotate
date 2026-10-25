//! Usage history per account: how much of its limits each account used, week by week, so the
//! user can tell which subscriptions earn their keep (`cc-rotate-ctl report`, and "Recent weeks"
//! in the app's account dialog).
//!
//! Everything here only reads the store, and only reports what was recorded:
//!
//! - **Accounts with a weekly limit:** every weekly window (`seven_day`, all models) that closed
//!   in the period, with the highest usage recorded for it. Usage within a window only grows, so
//!   that is its last reading, and a lower bound when the readings stop before the reset.
//! - **Accounts without one:** per calendar week (Monday to Monday, local time), how many
//!   distinct 5-hour windows were used and how many of them reached `swap_threshold_5h`.
//!
//! A week whose data is thin is marked as such (`THIN_SAMPLES`, `THIN_GAP_S`), and a week with
//! no readings at all has no numbers, never zeros.

use anyhow::Result;
use serde::Serialize;

use crate::scheduler::{self, FIVE_HOUR, SEVEN_DAY, WeeklyLimit};
use crate::store::{Account, Store, WindowPeak};
use crate::{plan, swap};

/// A week with fewer readings than this is thin.
pub const THIN_SAMPLES: usize = 3;
/// A weekly window whose last reading came more than this long before its reset is thin: the
/// account may have used more after it.
pub const THIN_GAP_S: i64 = 2 * 86_400;
/// Reset times within this of each other belong to one window (a reset time can move slightly
/// between readings, as in `scheduler::distinct_windows`).
const SAME_WINDOW_S: i64 = 3600;
pub const WEEK_S: i64 = 7 * 86_400;
const FIVE_HOUR_S: i64 = 5 * 3600;

/// Which history says how much use an account gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryKind {
    /// The account has a weekly limit: its weekly windows.
    Weekly,
    /// No weekly limit: its 5-hour windows per calendar week.
    FiveHour,
}

/// One weekly window that has closed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WeeklyWindow {
    /// When it reset (epoch seconds).
    pub resets_at: i64,
    /// The highest usage recorded for it.
    pub peak_pct: f64,
    /// Readings that reported it.
    pub samples: usize,
    /// Epoch ms.
    pub last_sample_at: i64,
    /// Few readings, or none in the days before the reset: it may have gone higher.
    pub thin: bool,
}

/// One calendar week of an account's 5-hour windows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CalendarWeek {
    /// Monday 00:00 local time (epoch seconds).
    pub starts_at: i64,
    /// The next Monday 00:00 (epoch seconds).
    pub ends_at: i64,
    /// The current week.
    pub in_progress: bool,
    /// Readings of the account taken this week.
    pub samples: usize,
    /// Distinct 5-hour windows first seen this week; `None` without a single reading.
    pub windows_used: Option<usize>,
    /// Of those, the ones that reached the 5-hour threshold; `None` without a single reading.
    pub windows_at_limit: Option<usize>,
    /// Few readings, or older than the history kept: the counts may be low.
    pub thin: bool,
    /// What the account's use this week would have cost at API prices, as Claude Code estimates
    /// it in the status line; `None` when cc-rotate counted none.
    pub api_value_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountHistory {
    pub account_id: i64,
    pub label: String,
    /// "Max 5x", "Team", "Pro", or the raw subscription type.
    pub plan: Option<String>,
    /// Whether it has a weekly limit, as the scheduler sees it.
    pub weekly_limit: WeeklyLimit,
    /// Which of the two histories describes the account.
    pub kind: HistoryKind,
    /// Weekly windows that closed in the period, newest first.
    pub weekly: Vec<WeeklyWindow>,
    /// Calendar weeks, newest first; the first is the current week.
    pub weeks: Vec<CalendarWeek>,
    /// The 5-hour threshold the counts use.
    pub threshold_5h: f64,
}

impl AccountHistory {
    /// What the account's use over the weeks reported would have cost at API prices, when
    /// cc-rotate counted any.
    pub fn api_value_usd(&self) -> Option<f64> {
        self.weeks
            .iter()
            .filter_map(|w| w.api_value_usd)
            .reduce(|a, b| a + b)
    }

    /// The average peak of the weekly windows, when there is one.
    pub fn average_weekly_peak(&self) -> Option<f64> {
        if self.weekly.is_empty() {
            return None;
        }
        Some(self.weekly.iter().map(|w| w.peak_pct).sum::<f64>() / self.weekly.len() as f64)
    }
}

/// Merge the peaks of reset times within `SAME_WINDOW_S` of each other into one window: the
/// highest usage, every reading, the latest reset time. `peaks` must be in ascending reset order
/// (as `Store::window_peaks` returns them).
pub fn merge_peaks(peaks: &[WindowPeak]) -> Vec<WindowPeak> {
    let mut merged: Vec<WindowPeak> = Vec::new();
    let mut first_reset = i64::MIN;
    for p in peaks {
        match merged.last_mut() {
            Some(m) if p.resets_at - first_reset <= SAME_WINDOW_S => {
                m.resets_at = m.resets_at.max(p.resets_at);
                m.peak_pct = m.peak_pct.max(p.peak_pct);
                m.samples += p.samples;
                m.first_sample_at = m.first_sample_at.min(p.first_sample_at);
                m.last_sample_at = m.last_sample_at.max(p.last_sample_at);
            }
            _ => {
                first_reset = p.resets_at;
                merged.push(p.clone());
            }
        }
    }
    merged
}

/// The weekly windows (already merged) that closed after `since_s` and by `now_s`, newest first.
/// A window that has not reset yet is still open and is left out.
pub fn closed_weekly_windows(peaks: &[WindowPeak], since_s: i64, now_s: i64) -> Vec<WeeklyWindow> {
    let mut out: Vec<WeeklyWindow> = peaks
        .iter()
        .filter(|p| p.resets_at > since_s && p.resets_at <= now_s)
        .map(|p| WeeklyWindow {
            resets_at: p.resets_at,
            peak_pct: p.peak_pct.clamp(0.0, 100.0),
            samples: p.samples,
            last_sample_at: p.last_sample_at,
            thin: p.samples < THIN_SAMPLES || p.resets_at - p.last_sample_at / 1000 > THIN_GAP_S,
        })
        .collect();
    out.reverse();
    out
}

/// The 5-hour windows (already merged) per calendar week. `bounds` are the weeks' `(start, end)`
/// in epoch seconds, newest first, and `samples` the account's readings in each. A window
/// belongs to the week it was first seen in. Weeks starting before `kept_from_s`, the oldest
/// reading the store still keeps, are thin.
pub fn calendar_weeks(
    five_hour: &[WindowPeak],
    bounds: &[(i64, i64)],
    samples: &[usize],
    threshold: f64,
    kept_from_s: i64,
    now_s: i64,
) -> Vec<CalendarWeek> {
    bounds
        .iter()
        .zip(samples)
        .map(|(&(starts_at, ends_at), &samples)| {
            let windows: Vec<&WindowPeak> = five_hour
                .iter()
                .filter(|p| {
                    let seen = p.first_sample_at.div_euclid(1000);
                    seen >= starts_at && seen < ends_at
                })
                .collect();
            let known = samples > 0 || !windows.is_empty();
            CalendarWeek {
                starts_at,
                ends_at,
                in_progress: starts_at <= now_s && now_s < ends_at,
                samples,
                windows_used: known.then_some(windows.len()),
                windows_at_limit: known
                    .then(|| windows.iter().filter(|p| p.peak_pct >= threshold).count()),
                thin: samples < THIN_SAMPLES || starts_at < kept_from_s,
                api_value_usd: None,
            }
        })
        .collect()
}

/// Every account's history, in the user's order. `bounds` are the calendar weeks to report,
/// newest first (`local_weeks`); weekly windows are reported for as many weeks back from now.
pub fn report(store: &Store, now_ms: i64, bounds: &[(i64, i64)]) -> Result<Vec<AccountHistory>> {
    store
        .accounts()?
        .iter()
        .map(|a| history(store, a, now_ms, bounds))
        .collect()
}

/// One account's history (see `report`).
pub fn account_report(
    store: &Store,
    account_id: i64,
    now_ms: i64,
    bounds: &[(i64, i64)],
) -> Result<AccountHistory> {
    let account = store
        .account(account_id)?
        .ok_or_else(|| anyhow::anyhow!("That account no longer exists."))?;
    history(store, &account, now_ms, bounds)
}

fn history(
    store: &Store,
    account: &Account,
    now_ms: i64,
    bounds: &[(i64, i64)],
) -> Result<AccountHistory> {
    let now_s = now_ms.div_euclid(1000);
    let settings = store.settings()?;
    // Samples older than the retention period may not be pruned yet; they don't count.
    let kept_from_ms = now_ms - settings.sample_retention_days.max(1) * 86_400_000;
    let kept_from_s = kept_from_ms.div_euclid(1000);

    let since_s = (now_s - bounds.len() as i64 * WEEK_S).max(kept_from_s);
    // A window's reset time can move by up to `SAME_WINDOW_S`, so read a little past both ends
    // to merge it whole; `closed_weekly_windows` then keeps only the period.
    let weekly_peaks = merge_peaks(&store.window_peaks(
        account.id,
        SEVEN_DAY,
        since_s - SAME_WINDOW_S,
        now_s + SAME_WINDOW_S,
        kept_from_ms,
    )?);
    let weekly = closed_weekly_windows(&weekly_peaks, since_s, now_s);

    let oldest = bounds.iter().map(|b| b.0).min().unwrap_or(now_s);
    let newest = bounds.iter().map(|b| b.1).max().unwrap_or(now_s);
    let five_hour = merge_peaks(&store.window_peaks(
        account.id,
        FIVE_HOUR,
        oldest,
        newest + FIVE_HOUR_S + SAME_WINDOW_S,
        kept_from_ms,
    )?);
    let samples = bounds
        .iter()
        .map(|&(start, end)| {
            store.sample_count(account.id, (start * 1000).max(kept_from_ms), end * 1000)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut weeks = calendar_weeks(
        &five_hour,
        bounds,
        &samples,
        settings.swap_threshold_5h,
        kept_from_s,
        now_s,
    );
    for week in &mut weeks {
        week.api_value_usd =
            store.account_cost_between(account.id, week.starts_at, week.ends_at)?;
    }

    let detected = swap::detect_weekly_limit(store, account.id, now_s)?;
    let weekly_limit = scheduler::weekly_limit(account.weekly_limit, detected);
    let kind = match weekly_limit {
        WeeklyLimit::Has => HistoryKind::Weekly,
        WeeklyLimit::Unknown if !weekly.is_empty() => HistoryKind::Weekly,
        _ => HistoryKind::FiveHour,
    };
    Ok(AccountHistory {
        account_id: account.id,
        label: account.label.clone(),
        plan: plan::label(
            account.subscription_type.as_deref(),
            account.rate_limit_tier.as_deref(),
        ),
        weekly_limit,
        kind,
        weekly,
        weeks,
        threshold_5h: settings.swap_threshold_5h,
    })
}

/// The current calendar week and the `weeks - 1` before it, newest first, as `(start, end)` in
/// epoch seconds. Weeks run from Monday 00:00 to the next Monday 00:00, local time.
pub fn local_weeks(now_s: i64, weeks: usize) -> Vec<(i64, i64)> {
    use chrono::{Datelike, Days, Local, TimeZone};
    let Some(now) = Local.timestamp_opt(now_s, 0).single() else {
        return Vec::new();
    };
    let monday = now.date_naive() - Days::new(u64::from(now.weekday().num_days_from_monday()));
    (0..weeks as u64)
        .filter_map(|i| {
            let start = monday.checked_sub_days(Days::new(7 * i))?;
            let end = start.checked_add_days(Days::new(7))?;
            Some((local_midnight(start), local_midnight(end)))
        })
        .collect()
}

/// Midnight at the start of `day`, local time; the first moment of the day where a clock
/// change skips midnight.
fn local_midnight(day: chrono::NaiveDate) -> i64 {
    use chrono::{Local, NaiveTime, TimeDelta, TimeZone};
    let midnight = day.and_time(NaiveTime::MIN);
    (0..=2)
        .find_map(|h| {
            Local
                .from_local_datetime(&(midnight + TimeDelta::hours(h)))
                .earliest()
        })
        .map_or_else(|| midnight.and_utc().timestamp(), |t| t.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AccountIdentity, AccountState, Sample, SampleWindow, WeeklyLimitSetting};

    const HOUR: i64 = 3600;
    const DAY: i64 = 86_400;
    /// A Monday, 00:00 UTC: 2026-10-19.
    const MONDAY: i64 = 1_792_368_000;

    fn peak(resets_at: i64, pct: f64, samples: usize, first_s: i64, last_s: i64) -> WindowPeak {
        WindowPeak {
            resets_at,
            peak_pct: pct,
            samples,
            first_sample_at: first_s * 1000,
            last_sample_at: last_s * 1000,
        }
    }

    fn store_with(label: &str) -> (Store, i64) {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .insert_account(
                label,
                &AccountIdentity {
                    account_uuid: Some(format!("{label}-uuid")),
                    ..Default::default()
                },
                AccountState::Parked,
            )
            .unwrap();
        (store, id)
    }

    fn record(store: &Store, id: i64, ts_s: i64, windows: &[(&str, f64, Option<i64>)]) {
        store
            .record_sample(&Sample {
                ts: ts_s * 1000,
                account_id: id,
                profile: "rotate".into(),
                session_id: None,
                model: None,
                windows: windows
                    .iter()
                    .map(|&(name, pct, resets_at)| SampleWindow {
                        name: name.into(),
                        pct,
                        resets_at,
                    })
                    .collect(),
            })
            .unwrap();
    }

    /// Four calendar weeks in UTC, newest first, the newest holding `now`.
    fn utc_weeks(now: i64, n: i64) -> Vec<(i64, i64)> {
        let this = MONDAY + (now - MONDAY).div_euclid(WEEK_S) * WEEK_S;
        (0..n)
            .map(|i| (this - i * WEEK_S, this - (i - 1) * WEEK_S))
            .collect()
    }

    #[test]
    fn merging_joins_reset_times_within_an_hour() {
        let merged = merge_peaks(&[
            peak(1000, 10.0, 2, 0, 10),
            peak(1000 + 60, 30.0, 1, 20, 30),
            peak(1000 + 3000, 25.0, 4, 5, 40),
            peak(1000 + 5 * HOUR, 5.0, 1, 50, 50),
        ]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], peak(4000, 30.0, 7, 0, 40));
        assert_eq!(merged[1].resets_at, 1000 + 5 * HOUR);
    }

    #[test]
    fn a_window_that_has_not_closed_is_left_out() {
        let now = MONDAY + 10 * DAY;
        let peaks = [
            peak(MONDAY, 40.0, 50, MONDAY - 6 * DAY, MONDAY - HOUR),
            peak(MONDAY + 7 * DAY, 70.0, 80, MONDAY, MONDAY + 7 * DAY - HOUR),
            // Still open: resets in four days.
            peak(MONDAY + 14 * DAY, 20.0, 30, MONDAY + 7 * DAY, now),
        ];
        let closed = closed_weekly_windows(&peaks, now - 4 * WEEK_S, now);
        let resets: Vec<i64> = closed.iter().map(|w| w.resets_at).collect();
        assert_eq!(resets, [MONDAY + 7 * DAY, MONDAY], "newest first");
        assert_eq!(closed[0].peak_pct, 70.0);
        assert!(closed.iter().all(|w| !w.thin));
    }

    #[test]
    fn a_reset_with_no_reading_near_it_is_thin_but_keeps_its_number() {
        let now = MONDAY + 8 * DAY;
        // Read once a day for three days early in the week, then nothing for four days.
        let peaks = [peak(MONDAY + 7 * DAY, 12.0, 3, MONDAY, MONDAY + 3 * DAY)];
        let closed = closed_weekly_windows(&peaks, now - 4 * WEEK_S, now);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].peak_pct, 12.0, "the recorded peak, not a guess");
        assert!(closed[0].thin);

        // Read up to the last day: enough readings, not thin.
        let peaks = [peak(MONDAY + 7 * DAY, 12.0, 3, MONDAY, MONDAY + 6 * DAY)];
        assert!(!closed_weekly_windows(&peaks, now - 4 * WEEK_S, now)[0].thin);

        // Two readings, the last right before the reset: still thin.
        let peaks = [peak(
            MONDAY + 7 * DAY,
            12.0,
            2,
            MONDAY,
            MONDAY + 7 * DAY - HOUR,
        )];
        assert!(closed_weekly_windows(&peaks, now - 4 * WEEK_S, now)[0].thin);
    }

    #[test]
    fn windows_before_the_period_are_left_out() {
        let now = MONDAY + 30 * DAY;
        let peaks = [
            peak(MONDAY, 99.0, 9, MONDAY - 6 * DAY, MONDAY - HOUR),
            peak(
                MONDAY + 28 * DAY,
                5.0,
                9,
                MONDAY + 21 * DAY,
                MONDAY + 28 * DAY - HOUR,
            ),
        ];
        let closed = closed_weekly_windows(&peaks, now - 2 * WEEK_S, now);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].peak_pct, 5.0);
    }

    #[test]
    fn five_hour_windows_are_counted_per_calendar_week() {
        let now = MONDAY + 2 * DAY;
        let bounds = utc_weeks(now, 3);
        let five = [
            // This week: two windows, one at the limit.
            peak(
                MONDAY + 6 * HOUR,
                99.0,
                10,
                MONDAY + HOUR,
                MONDAY + 5 * HOUR,
            ),
            peak(
                MONDAY + DAY + 6 * HOUR,
                40.0,
                10,
                MONDAY + DAY + HOUR,
                MONDAY + DAY + 5 * HOUR,
            ),
            // Last week: one at the limit. It started Sunday night and resets Monday, so it
            // belongs to the week it was first seen in.
            peak(MONDAY + 3 * HOUR, 98.0, 4, MONDAY - 2 * HOUR, MONDAY + HOUR),
        ];
        let weeks = calendar_weeks(&five, &bounds, &[20, 4, 0], 98.0, 0, now);
        assert_eq!(weeks.len(), 3);
        assert!(weeks[0].in_progress && !weeks[1].in_progress);
        assert_eq!(
            (weeks[0].windows_used, weeks[0].windows_at_limit),
            (Some(2), Some(1))
        );
        assert_eq!(
            (weeks[1].windows_used, weeks[1].windows_at_limit),
            (Some(1), Some(1))
        );
        assert!(!weeks[0].thin && !weeks[1].thin);
        // No reading at all: no numbers, not zeros.
        assert_eq!(
            (weeks[2].windows_used, weeks[2].windows_at_limit),
            (None, None)
        );
        assert!(weeks[2].thin);
    }

    #[test]
    fn a_week_with_readings_but_no_window_used_is_zero_and_a_sparse_one_thin() {
        let now = MONDAY + 2 * DAY;
        let bounds = utc_weeks(now, 2);
        let weeks = calendar_weeks(&[], &bounds, &[40, 2], 98.0, 0, now);
        assert_eq!(weeks[0].windows_used, Some(0));
        assert!(!weeks[0].thin);
        assert_eq!(weeks[1].windows_used, Some(0));
        assert!(weeks[1].thin, "two readings in a week");
    }

    #[test]
    fn weeks_older_than_the_history_kept_are_thin() {
        let now = MONDAY + 2 * DAY;
        let bounds = utc_weeks(now, 2);
        let weeks = calendar_weeks(&[], &bounds, &[40, 40], 98.0, MONDAY - 3 * DAY, now);
        assert!(!weeks[0].thin);
        assert!(weeks[1].thin);
    }

    #[test]
    fn an_account_without_data_reports_nothing() {
        let (store, id) = store_with("idle");
        let now = MONDAY + 2 * DAY;
        let h = account_report(&store, id, now * 1000, &utc_weeks(now, 4)).unwrap();
        assert!(h.weekly.is_empty());
        assert_eq!(h.weeks.len(), 4);
        assert!(
            h.weeks
                .iter()
                .all(|w| w.windows_used.is_none() && w.samples == 0)
        );
        assert_eq!(h.weekly_limit, WeeklyLimit::Unknown);
        assert_eq!(h.kind, HistoryKind::FiveHour);
        assert_eq!(h.average_weekly_peak(), None);
    }

    #[test]
    fn weekly_account_from_the_store() {
        let (store, id) = store_with("main");
        // A fixed past moment: `now` is passed in, and retention counts back from it.
        let now = MONDAY + 2 * DAY;
        let reset1 = MONDAY - 5 * DAY;
        let reset2 = MONDAY + 2 * DAY - HOUR;
        let reset3 = MONDAY + 9 * DAY - HOUR;
        // Window 1: read through the week, the reset time moving by a minute between sources.
        for (i, pct) in [5.0, 20.0, 34.0].iter().enumerate() {
            let ts = reset1 - (3 - i as i64) * DAY;
            let reset = if i == 1 { reset1 + 60 } else { reset1 };
            record(
                &store,
                id,
                ts,
                &[
                    ("five_hour", 10.0, Some(ts + HOUR)),
                    ("seven_day", *pct, Some(reset)),
                ],
            );
        }
        // Window 2: read once.
        record(
            &store,
            id,
            reset2 - 4 * DAY,
            &[("seven_day", 1.0, Some(reset2))],
        );
        // Window 3: still open.
        record(&store, id, now - HOUR, &[("seven_day", 50.0, Some(reset3))]);

        let h = account_report(&store, id, now * 1000, &utc_weeks(now, 4)).unwrap();
        assert_eq!(h.weekly_limit, WeeklyLimit::Has);
        assert_eq!(h.kind, HistoryKind::Weekly);
        let got: Vec<(i64, f64, usize, bool)> = h
            .weekly
            .iter()
            .map(|w| (w.resets_at, w.peak_pct, w.samples, w.thin))
            .collect();
        assert_eq!(
            got,
            [(reset2, 1.0, 1, true), (reset1 + 60, 34.0, 3, false)],
            "the open window is left out, the moved reset time merged"
        );
        assert_eq!(h.average_weekly_peak(), Some(17.5));
    }

    #[test]
    fn no_weekly_limit_account_from_the_store() {
        let (store, id) = store_with("account3");
        let now = MONDAY + 3 * DAY;
        // Two 5-hour windows in each of two days this week, read every 30 minutes; the
        // second of each day reaches 98%.
        for day in 0..2 {
            for w in 0..2 {
                let start = MONDAY + day * DAY + 8 * HOUR + w * 5 * HOUR;
                for step in 0..10 {
                    let ts = start + step * 1800;
                    let pct = if w == 1 {
                        10.0 * step as f64 + 8.0
                    } else {
                        3.0 * step as f64
                    };
                    record(
                        &store,
                        id,
                        ts,
                        &[("five_hour", pct, Some(start + 5 * HOUR))],
                    );
                }
            }
        }
        let h = account_report(&store, id, now * 1000, &utc_weeks(now, 2)).unwrap();
        assert_eq!(
            h.weekly_limit,
            WeeklyLimit::None,
            "four 5-hour windows, no weekly one"
        );
        assert_eq!(h.kind, HistoryKind::FiveHour);
        assert_eq!(h.weeks[0].samples, 40);
        assert_eq!(h.weeks[0].windows_used, Some(4));
        assert_eq!(h.weeks[0].windows_at_limit, Some(2));
        assert_eq!(h.weeks[1].windows_used, None);
    }

    #[test]
    fn the_users_setting_decides_the_kind() {
        let (store, id) = store_with("a");
        store.set_weekly_limit(id, WeeklyLimitSetting::Yes).unwrap();
        let now = MONDAY + 2 * DAY;
        let h = account_report(&store, id, now * 1000, &utc_weeks(now, 1)).unwrap();
        assert_eq!(h.kind, HistoryKind::Weekly);
        store.set_weekly_limit(id, WeeklyLimitSetting::No).unwrap();
        let h = account_report(&store, id, now * 1000, &utc_weeks(now, 1)).unwrap();
        assert_eq!(h.kind, HistoryKind::FiveHour);
    }

    #[test]
    fn readings_older_than_retention_do_not_count() {
        let (store, id) = store_with("old");
        store
            .set_setting("sample_retention_days", Some("7"))
            .unwrap();
        let now = MONDAY + 20 * DAY;
        // The window reset within the 7 days kept, but its only reading is older than that.
        let reset = MONDAY + 14 * DAY;
        record(
            &store,
            id,
            MONDAY + 8 * DAY,
            &[("seven_day", 80.0, Some(reset))],
        );
        let h = account_report(&store, id, now * 1000, &utc_weeks(now, 4)).unwrap();
        assert!(h.weekly.is_empty(), "pruned at the next prune anyway");
        // A reading within the 7 days counts.
        record(
            &store,
            id,
            MONDAY + 13 * DAY + HOUR,
            &[("seven_day", 85.0, Some(reset))],
        );
        let h = account_report(&store, id, now * 1000, &utc_weeks(now, 4)).unwrap();
        assert_eq!(h.weekly.len(), 1);
        assert_eq!((h.weekly[0].peak_pct, h.weekly[0].samples), (85.0, 1));
    }

    #[test]
    fn reports_never_write() {
        let (store, id) = store_with("main");
        record(
            &store,
            id,
            MONDAY,
            &[("seven_day", 10.0, Some(MONDAY + DAY))],
        );
        let before = store.conn().total_changes();
        let now = MONDAY + 2 * DAY;
        report(&store, now * 1000, &utc_weeks(now, 4)).unwrap();
        assert_eq!(store.conn().total_changes(), before);
    }

    #[test]
    fn local_weeks_start_on_monday_and_go_back_a_week_at_a_time() {
        let now = crate::time::now_s();
        let weeks = local_weeks(now, 4);
        assert_eq!(weeks.len(), 4);
        assert!(weeks[0].0 <= now && now < weeks[0].1, "the first holds now");
        for pair in weeks.windows(2) {
            assert_eq!(pair[1].1, pair[0].0, "back to back");
        }
        for (start, end) in &weeks {
            // A week is 7 days, give or take a clock change.
            assert!((end - start - WEEK_S).abs() <= 2 * HOUR, "{start} {end}");
            use chrono::{Datelike, Local, TimeZone, Timelike};
            let local = Local.timestamp_opt(*start, 0).unwrap();
            assert_eq!(local.weekday(), chrono::Weekday::Mon);
            assert!(local.hour() <= 2);
        }
    }
}
