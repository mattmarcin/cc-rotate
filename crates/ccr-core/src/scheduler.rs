//! When to swap, and which account to swap to. Pure functions over store data.
//!
//! Limits are modelled generically. A sample holds every usage window the data source reported
//! (Claude Code's status line input: `five_hour`, `seven_day`, and any window it adds later),
//! and `classify` says what each one limits: a 5-hour session window or a weekly window, on all
//! models or on one model family. An account is ineligible while any window that applies to the
//! model in use is at or above its threshold.
//!
//! Ranking, best first:
//! 1. With `prefer_no_weekly_limit` on: accounts without a weekly limit, then accounts whose
//!    weekly limit is not known yet, then accounts with one.
//! 2. The user's order (`position`).
//! 3. The most room before a threshold, then the label.
//!
//! Only the first two decide whether a better account is worth switching back to; the third
//! only breaks ties when picking, so equally ranked accounts never swap just to balance.

use std::cmp::Ordering;

use serde::Serialize;

use crate::store::{Account, AccountState, Sample, Settings, WeeklyLimitSetting};
use crate::time;

pub const FIVE_HOUR: &str = "five_hour";
pub const SEVEN_DAY: &str = "seven_day";

/// Model families a window name can be scoped to (`seven_day_opus`). Matched against the
/// session's model id, which contains the family name (`claude-opus-4-1`).
const MODEL_FAMILIES: &[&str] = &["opus", "sonnet", "haiku", "fable"];

/// The model family a model's display name or id belongs to ("Opus 4.5" and `claude-opus-4-5`
/// are `opus`), when it is one cc-rotate knows.
pub fn model_family(model: &str) -> Option<&'static str> {
    let model = model.to_ascii_lowercase();
    MODEL_FAMILIES.iter().copied().find(|f| model.contains(f))
}

/// A better ranked account is only switched back to while every window that applies is at
/// least this many points below its threshold, so it is not left again a moment later.
pub const SWITCH_BACK_MARGIN: f64 = 5.0;

/// "No weekly limit" needs this many distinct 5-hour windows, within `NO_LIMIT_LOOKBACK_S`,
/// without a single weekly window. A real request on a capped account reports the weekly window
/// alongside the 5-hour one, so two windows' worth of requests without one is strong evidence;
/// a single window could be a status line that only ran before the first request.
pub const NO_LIMIT_SESSIONS: usize = 2;
pub const NO_LIMIT_LOOKBACK_S: i64 = 8 * 86_400;

/// Minimum gap between swap requests that ticks spawn.
pub const REQUEST_INTERVAL_MS: i64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    Session,
    Weekly,
    Other,
}

/// What a usage window limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WindowSpec {
    pub kind: WindowKind,
    /// The model family the window is limited to; `None` for all models.
    pub model: Option<String>,
}

impl WindowSpec {
    /// Whether this window limits `model`. A model-scoped window applies when the model belongs
    /// to its family, and also when the model is not known, since hitting a wall costs more
    /// than swapping early.
    pub fn applies_to(&self, model: Option<&str>) -> bool {
        match (&self.model, model) {
            (None, _) | (Some(_), None) => true,
            (Some(family), Some(m)) => m.to_ascii_lowercase().contains(family.as_str()),
        }
    }

    pub fn all_models(&self) -> bool {
        self.model.is_none()
    }
}

/// Classify a window by the name the data source gives it. Unknown names are treated as a
/// limit on all models, so a new limit system blocks too early rather than too late.
pub fn classify(name: &str) -> WindowSpec {
    let family = |rest: &str| {
        MODEL_FAMILIES
            .iter()
            .find(|f| rest.eq_ignore_ascii_case(f))
            .map(|f| f.to_string())
    };
    let (kind, model) = match name {
        FIVE_HOUR => (WindowKind::Session, None),
        SEVEN_DAY => (WindowKind::Weekly, None),
        _ => match (
            name.strip_prefix("seven_day_"),
            name.strip_prefix("five_hour_"),
        ) {
            (Some(rest), _) => (WindowKind::Weekly, family(rest)),
            (_, Some(rest)) => (WindowKind::Session, family(rest)),
            _ => (WindowKind::Other, None),
        },
    };
    WindowSpec { kind, model }
}

/// A short name for a window, to use mid-sentence: "5-hour", "weekly", "Opus weekly".
pub fn window_label(name: &str) -> String {
    let spec = classify(name);
    let base = match spec.kind {
        WindowKind::Session => "5-hour",
        WindowKind::Weekly => "weekly",
        WindowKind::Other => return name.replace('_', " "),
    };
    match (&spec.model, name) {
        (Some(family), _) => {
            let mut chars = family.chars();
            let first = chars.next().map(|c| c.to_ascii_uppercase());
            format!("{}{} {base}", first.unwrap_or_default(), chars.as_str())
        }
        (None, FIVE_HOUR | SEVEN_DAY) => base.to_string(),
        (None, _) => name.replace('_', " "),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// For 5-hour windows (`swap_threshold_5h`).
    pub session: f64,
    /// For weekly windows, on all models or on one (`swap_threshold_7d`).
    pub weekly: f64,
}

impl Thresholds {
    /// Windows of an unknown kind get the stricter of the two thresholds.
    pub fn for_kind(&self, kind: WindowKind) -> f64 {
        match kind {
            WindowKind::Session => self.session,
            WindowKind::Weekly => self.weekly,
            WindowKind::Other => self.session.min(self.weekly),
        }
    }
}

impl From<&Settings> for Thresholds {
    fn from(s: &Settings) -> Self {
        Self {
            session: s.swap_threshold_5h,
            weekly: s.swap_threshold_7d,
        }
    }
}

/// Usage for one window: the last sampled percentage, or 0 once the window has reset.
pub fn effective(pct: Option<f64>, resets_at_s: Option<i64>, now_s: i64) -> f64 {
    match (pct, resets_at_s) {
        (_, Some(reset)) if reset <= now_s => 0.0,
        (Some(p), _) => p.clamp(0.0, 100.0),
        (None, _) => 0.0,
    }
}

/// One window of an account's latest reading, as it stands now.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowUsage {
    pub name: String,
    pub label: String,
    pub kind: WindowKind,
    pub model: Option<String>,
    /// The last reading, or 0 once the window has reset.
    pub pct: f64,
    /// Epoch seconds; in the past once the window has reset.
    pub resets_at: Option<i64>,
    pub threshold: f64,
    /// Whether the window limits the model the rotation profile is using.
    pub applies: bool,
}

impl WindowUsage {
    pub fn blocking(&self) -> bool {
        self.applies && self.pct >= self.threshold
    }

    pub fn room(&self) -> f64 {
        self.threshold - self.pct
    }

    pub fn is_weekly_all_models(&self) -> bool {
        self.kind == WindowKind::Weekly && self.model.is_none()
    }
}

/// The windows of an account's latest sample. A window that the latest sample does not have is
/// not reported by the data source any more, so it does not count.
pub fn window_usage(
    sample: Option<&Sample>,
    now_s: i64,
    t: Thresholds,
    model: Option<&str>,
) -> Vec<WindowUsage> {
    let Some(sample) = sample else {
        return Vec::new();
    };
    sample
        .windows
        .iter()
        .map(|w| {
            let spec = classify(&w.name);
            WindowUsage {
                label: window_label(&w.name),
                pct: effective(Some(w.pct), w.resets_at, now_s),
                resets_at: w.resets_at,
                threshold: t.for_kind(spec.kind),
                applies: spec.applies_to(model),
                kind: spec.kind,
                model: spec.model,
                name: w.name.clone(),
            }
        })
        .collect()
}

/// Whether an account has a weekly limit on all models. The order is the ranking order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WeeklyLimit {
    None,
    Unknown,
    Has,
}

/// What an account's usage history says about its weekly limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WeeklyEvidence {
    /// A weekly window on all models was recorded within the retention period.
    pub weekly_seen: bool,
    /// Distinct 5-hour windows recorded within `NO_LIMIT_LOOKBACK_S`.
    pub session_windows: usize,
}

impl WeeklyEvidence {
    /// From every window name recorded for the account, and the reset times recorded for its
    /// all-models 5-hour windows within the lookback.
    pub fn from_history(names: &[String], session_resets: &[i64]) -> Self {
        Self {
            weekly_seen: names.iter().any(|n| {
                let spec = classify(n);
                spec.kind == WindowKind::Weekly && spec.all_models()
            }),
            session_windows: distinct_windows(session_resets),
        }
    }
}

/// Count windows from their reset times, treating times within an hour as the same window
/// (a reset time can move slightly between readings).
pub fn distinct_windows(resets_s: &[i64]) -> usize {
    let mut sorted = resets_s.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut count = 0;
    let mut last: Option<i64> = None;
    for r in sorted {
        if last.is_none_or(|l| r - l > 3600) {
            count += 1;
            last = Some(r);
        }
    }
    count
}

pub fn detect_weekly_limit(e: WeeklyEvidence) -> WeeklyLimit {
    if e.weekly_seen {
        WeeklyLimit::Has
    } else if e.session_windows >= NO_LIMIT_SESSIONS {
        WeeklyLimit::None
    } else {
        WeeklyLimit::Unknown
    }
}

pub fn weekly_limit(setting: WeeklyLimitSetting, detected: WeeklyLimit) -> WeeklyLimit {
    match setting {
        WeeklyLimitSetting::Auto => detected,
        WeeklyLimitSetting::Yes => WeeklyLimit::Has,
        WeeklyLimitSetting::No => WeeklyLimit::None,
    }
}

/// Why a swap is wanted. The string form is what events record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    FiveHour,
    Weekly,
    /// A window of a kind cc-rotate does not know reached its threshold.
    OtherLimit,
    /// A better ranked account is usable again (switch back).
    PreferredAvailable,
    StopFailure,
    DeadLogin,
    Manual,
    Login,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FiveHour => "threshold_5h",
            Self::Weekly => "threshold_7d",
            Self::OtherLimit => "threshold_other",
            Self::PreferredAvailable => "preferred_available",
            Self::StopFailure => "stop_failure",
            Self::DeadLogin => "dead_login",
            Self::Manual => "manual",
            Self::Login => "login",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "threshold_5h" => Self::FiveHour,
            "threshold_7d" => Self::Weekly,
            "threshold_other" => Self::OtherLimit,
            "preferred_available" => Self::PreferredAvailable,
            "stop_failure" => Self::StopFailure,
            "dead_login" => Self::DeadLogin,
            "manual" => Self::Manual,
            "login" => Self::Login,
            _ => return None,
        })
    }

    /// Usage-driven triggers respect the cooldown; the others are urgent or explicit.
    pub fn respects_cooldown(self) -> bool {
        self.is_threshold() || self == Self::PreferredAvailable
    }

    /// A window of the active account reached its threshold.
    pub fn is_threshold(self) -> bool {
        matches!(self, Self::FiveHour | Self::Weekly | Self::OtherLimit)
    }
}

/// An account with what the scheduler knows about it right now.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub account: Account,
    pub windows: Vec<WindowUsage>,
    pub weekly_detected: WeeklyLimit,
    /// `weekly_detected`, or the user's override.
    pub weekly_limit: WeeklyLimit,
    /// Place in the user's order, from 0; accounts at the same position share a place.
    pub order: usize,
}

impl Candidate {
    /// The window that keeps this account from being used, if any: a weekly one before a
    /// 5-hour one, and of those the one that resets last.
    pub fn blocking(&self) -> Option<&WindowUsage> {
        let weight = |k: WindowKind| match k {
            WindowKind::Weekly => 2,
            WindowKind::Other => 1,
            WindowKind::Session => 0,
        };
        self.windows
            .iter()
            .filter(|w| w.blocking())
            .max_by_key(|w| (weight(w.kind), w.resets_at.unwrap_or(0)))
    }

    /// When every blocking window has reset (epoch seconds).
    pub fn blocked_until(&self) -> Option<i64> {
        self.windows
            .iter()
            .filter(|w| w.blocking())
            .filter_map(|w| w.resets_at)
            .max()
    }

    /// Points left before the nearest threshold among the windows that apply.
    pub fn room(&self, t: Thresholds) -> f64 {
        self.windows
            .iter()
            .filter(|w| w.applies)
            .map(WindowUsage::room)
            .reduce(f64::min)
            .unwrap_or(t.session)
    }

    /// The weekly window on all models, when the latest reading has one.
    pub fn weekly(&self) -> Option<&WindowUsage> {
        self.windows.iter().find(|w| w.is_weekly_all_models())
    }

    /// The 5-hour window on all models, when the latest reading has one.
    pub fn five_hour(&self) -> Option<&WindowUsage> {
        self.windows
            .iter()
            .find(|w| w.kind == WindowKind::Session && w.model.is_none())
    }

    pub fn usable(&self) -> bool {
        matches!(
            self.account.state,
            AccountState::Active | AccountState::Parked
        )
    }

    /// Parked and below every threshold that applies: the scheduler may pick it.
    pub fn eligible(&self) -> bool {
        self.account.state == AccountState::Parked && self.blocking().is_none()
    }

    fn has_margin(&self) -> bool {
        self.windows
            .iter()
            .filter(|w| w.applies)
            .all(|w| w.pct <= w.threshold - SWITCH_BACK_MARGIN)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    pub thresholds: Thresholds,
    pub prefer_no_weekly_limit: bool,
}

impl From<&Settings> for Policy {
    fn from(s: &Settings) -> Self {
        Self {
            thresholds: Thresholds::from(s),
            prefer_no_weekly_limit: s.prefer_no_weekly_limit,
        }
    }
}

impl Policy {
    /// 0 for no weekly limit, 1 for not known yet, 2 for a weekly limit; all 0 when the
    /// preference is off.
    pub fn tier(&self, c: &Candidate) -> u8 {
        if !self.prefer_no_weekly_limit {
            return 0;
        }
        match c.weekly_limit {
            WeeklyLimit::None => 0,
            WeeklyLimit::Unknown => 1,
            WeeklyLimit::Has => 2,
        }
    }

    /// The rank that decides switching back: lower is better. Room is deliberately not part
    /// of it.
    pub fn rank(&self, c: &Candidate) -> (u8, i64) {
        (self.tier(c), c.account.position)
    }

    fn pick_order(&self, a: &Candidate, b: &Candidate) -> Ordering {
        self.rank(a)
            .cmp(&self.rank(b))
            .then(b.room(self.thresholds).total_cmp(&a.room(self.thresholds)))
            .then_with(|| a.account.label.cmp(&b.account.label))
    }
}

/// The best eligible account, excluding `exclude`.
pub fn pick(candidates: &[Candidate], policy: Policy, exclude: Option<i64>) -> Option<&Candidate> {
    candidates
        .iter()
        .filter(|c| c.eligible() && Some(c.account.id) != exclude)
        .min_by(|a, b| policy.pick_order(a, b))
}

/// The account to switch back to: the best eligible account that ranks strictly above the
/// active one and has `SWITCH_BACK_MARGIN` to spare. `None` while the active account is over a
/// threshold, since the threshold trigger handles that.
pub fn switch_back_target(candidates: &[Candidate], policy: Policy) -> Option<&Candidate> {
    let active = candidates
        .iter()
        .find(|c| c.account.state == AccountState::Active)?;
    if active.blocking().is_some() {
        return None;
    }
    let mut better: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.eligible() && policy.rank(c) < policy.rank(active) && c.has_margin())
        .collect();
    better.sort_by(|a, b| policy.pick_order(a, b));
    better.first().copied()
}

/// The threshold trigger for the active account's live usage, if any.
pub fn threshold_trigger(active: &Candidate) -> Option<(Reason, &WindowUsage)> {
    let window = active.blocking()?;
    let reason = match window.kind {
        WindowKind::Weekly => Reason::Weekly,
        WindowKind::Session => Reason::FiveHour,
        WindowKind::Other => Reason::OtherLimit,
    };
    Some((reason, window))
}

/// What a tick knows about time, for throttling.
#[derive(Debug, Clone, Copy, Default)]
pub struct Timing {
    pub now_ms: i64,
    pub last_swap_ms: i64,
    /// When the active account became active.
    pub active_since_ms: i64,
    pub last_request_ms: i64,
    pub cooldown_ms: i64,
    pub dwell_ms: i64,
    /// Every reading of the active account since it became active had it over a threshold: it
    /// was already used up when it was picked (`swap::arrived_over_limit`).
    pub arrived_over_limit: bool,
}

impl Timing {
    fn throttled(&self, reason: Reason) -> bool {
        self.now_ms - self.last_request_ms < REQUEST_INTERVAL_MS
            || (reason.respects_cooldown() && self.cooling_down() && !self.leaves_at_once(reason))
    }

    pub fn cooling_down(&self) -> bool {
        self.now_ms - self.last_swap_ms < self.cooldown_ms
    }

    /// An account that was already over a threshold when it was swapped to is left without
    /// waiting out the cooldown. The cooldown is there to stop accounts trading places, and this
    /// cannot: the account swapped away from was itself over a threshold, or ranked lower.
    pub fn leaves_at_once(&self, reason: Reason) -> bool {
        self.arrived_over_limit && reason.is_threshold()
    }

    pub fn dwell_done(&self) -> bool {
        self.now_ms - self.active_since_ms >= self.dwell_ms
    }
}

/// Whether a tick should request a swap now. `thresholds: false` leaves out the threshold
/// triggers (the app's background sync, which has no live reading of its own).
pub fn decide(
    candidates: &[Candidate],
    policy: Policy,
    timing: Timing,
    dead_login: bool,
    thresholds: bool,
) -> Option<Reason> {
    let active = candidates
        .iter()
        .find(|c| c.account.state == AccountState::Active)?;
    if dead_login {
        return Some(Reason::DeadLogin);
    }
    if let Some((reason, _)) = threshold_trigger(active) {
        return (thresholds && !timing.throttled(reason)).then_some(reason);
    }
    let reason = Reason::PreferredAvailable;
    if timing.throttled(reason) || !timing.dwell_done() {
        return None;
    }
    switch_back_target(candidates, policy).map(|_| reason)
}

/// One account in the pick order, with a short reason for its place.
#[derive(Debug, Clone, Serialize)]
pub struct Ranked {
    pub account_id: i64,
    pub label: String,
    /// Place in the pick order, from 1, for accounts that can be used now (the active one
    /// included); `None` for accounts over a limit, paused, or needing a login.
    pub rank: Option<usize>,
    pub eligible: bool,
    pub reason: String,
}

/// Every account, in the order the scheduler would use them, with a reason for each.
pub fn ranking(candidates: &[Candidate], policy: Policy, now_s: i64) -> Vec<Ranked> {
    let mut open: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.usable() && c.blocking().is_none())
        .collect();
    open.sort_by(|a, b| policy.pick_order(a, b));
    let mut blocked: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.usable() && c.blocking().is_some())
        .collect();
    blocked.sort_by_key(|c| (c.blocked_until().unwrap_or(i64::MAX), c.order));
    let rest = candidates.iter().filter(|c| !c.usable());

    let mut out = Vec::with_capacity(candidates.len());
    for (i, c) in open.iter().enumerate() {
        let tied = open
            .iter()
            .any(|o| o.account.id != c.account.id && policy.rank(o) == policy.rank(c));
        out.push(Ranked {
            account_id: c.account.id,
            label: c.account.label.clone(),
            rank: Some(i + 1),
            eligible: c.eligible(),
            reason: open_reason(c, policy, tied),
        });
    }
    for c in blocked.into_iter().chain(rest) {
        out.push(Ranked {
            account_id: c.account.id,
            label: c.account.label.clone(),
            rank: None,
            eligible: false,
            reason: closed_reason(c, now_s),
        });
    }
    out
}

fn open_reason(c: &Candidate, policy: Policy, tied: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    if policy.prefer_no_weekly_limit {
        parts.push(match (c.weekly_limit, c.weekly()) {
            (WeeklyLimit::None, _) => "no weekly limit".into(),
            (WeeklyLimit::Unknown, _) => "weekly limit not known yet".into(),
            (WeeklyLimit::Has, Some(w)) => format!("weekly limit {:.0}% used", w.pct),
            (WeeklyLimit::Has, None) => "has a weekly limit".into(),
        });
    }
    parts.push(format!("{} in your order", ordinal(c.order + 1)));
    if tied {
        parts.push(format!(
            "tied, {:.0}% room left",
            c.room(policy.thresholds).max(0.0)
        ));
    }
    parts.join(", ")
}

fn closed_reason(c: &Candidate, now_s: i64) -> String {
    match c.account.state {
        AccountState::Paused => return "paused".into(),
        AccountState::NeedsLogin => return "needs a new login".into(),
        _ => {}
    }
    match c.blocking() {
        Some(w) => {
            let mut text = format!("{} limit at {:.0}%", w.label, w.pct);
            if let Some(until) = c.blocked_until().filter(|&u| u > now_s) {
                text.push_str(&format!(", resets in {}", time::span(until - now_s)));
            }
            text
        }
        None => "unavailable".into(),
    }
}

/// "1st", "2nd", "3rd", "11th", "22nd".
pub fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SampleWindow;

    const T: Thresholds = Thresholds {
        session: 98.0,
        weekly: 98.0,
    };
    const POLICY: Policy = Policy {
        thresholds: T,
        prefer_no_weekly_limit: true,
    };
    const NOW: i64 = 1_800_000_000;

    fn acct(id: i64, label: &str, state: AccountState, position: i64) -> Account {
        Account {
            id,
            label: label.into(),
            email: None,
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            subscription_type: None,
            rate_limit_tier: None,
            state,
            position,
            weekly_limit: WeeklyLimitSetting::Auto,
            added_at: 0,
            last_login_at: None,
        }
    }

    fn sample(windows: &[(&str, f64, i64)]) -> Sample {
        Sample {
            ts: 0,
            account_id: 0,
            profile: "rotate".into(),
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

    fn cand(account: Account, weekly: WeeklyLimit, windows: &[(&str, f64, i64)]) -> Candidate {
        Candidate {
            order: account.position as usize,
            windows: window_usage(Some(&sample(windows)), NOW, T, Some("claude-opus-4-1")),
            weekly_detected: weekly,
            weekly_limit: weekly,
            account,
        }
    }

    #[test]
    fn effective_resets_to_zero() {
        assert_eq!(effective(Some(80.0), Some(100), 100), 0.0);
        assert_eq!(effective(Some(80.0), Some(101), 100), 80.0);
        assert_eq!(effective(None, None, 100), 0.0);
        assert_eq!(effective(Some(120.0), None, 0), 100.0);
    }

    #[test]
    fn window_classification_and_applicability() {
        let five = classify("five_hour");
        assert_eq!((five.kind, five.model.clone()), (WindowKind::Session, None));
        let week = classify("seven_day");
        assert_eq!((week.kind, week.model.clone()), (WindowKind::Weekly, None));
        let opus = classify("seven_day_opus");
        assert_eq!(opus.kind, WindowKind::Weekly);
        assert_eq!(opus.model.as_deref(), Some("opus"));
        let surface = classify("seven_day_oauth_apps");
        assert_eq!(
            (surface.kind, surface.model.clone()),
            (WindowKind::Weekly, None),
            "a weekly window that names no model family limits every model"
        );
        let other = classify("spend_limit");
        assert_eq!((other.kind, other.model.clone()), (WindowKind::Other, None));

        // All-models windows always apply; scoped ones only to their family, or when the
        // model is unknown.
        assert!(week.applies_to(Some("claude-sonnet-4-5")));
        assert!(week.applies_to(None));
        assert!(opus.applies_to(Some("claude-opus-4-1-20250805")));
        assert!(opus.applies_to(Some("Claude-OPUS-5")));
        assert!(!opus.applies_to(Some("claude-sonnet-4-5")));
        assert!(opus.applies_to(None));
        assert!(other.applies_to(Some("claude-haiku-4-5")));

        assert_eq!(T.for_kind(WindowKind::Other), 98.0);
        let mixed = Thresholds {
            session: 90.0,
            weekly: 95.0,
        };
        assert_eq!(mixed.for_kind(WindowKind::Session), 90.0);
        assert_eq!(mixed.for_kind(WindowKind::Weekly), 95.0);
        assert_eq!(mixed.for_kind(WindowKind::Other), 90.0);

        assert_eq!(window_label("five_hour"), "5-hour");
        assert_eq!(window_label("seven_day"), "weekly");
        assert_eq!(window_label("seven_day_opus"), "Opus weekly");
        assert_eq!(window_label("spend_limit"), "spend limit");
    }

    #[test]
    fn a_scoped_window_only_blocks_its_own_model() {
        let s = sample(&[
            ("five_hour", 10.0, NOW + 3600),
            ("seven_day_opus", 99.0, NOW + 86_400),
        ]);
        let on_sonnet = window_usage(Some(&s), NOW, T, Some("claude-sonnet-4-5"));
        let on_opus = window_usage(Some(&s), NOW, T, Some("claude-opus-4-1"));
        assert!(!on_sonnet.iter().any(WindowUsage::blocking));
        assert!(on_opus.iter().any(WindowUsage::blocking));
    }

    #[test]
    fn weekly_limit_detection_table() {
        let e = |weekly_seen, session_windows| WeeklyEvidence {
            weekly_seen,
            session_windows,
        };
        let cases = [
            (e(false, 0), WeeklyLimit::Unknown),
            (e(false, 1), WeeklyLimit::Unknown),
            (e(false, 2), WeeklyLimit::None),
            (e(false, 9), WeeklyLimit::None),
            (e(true, 0), WeeklyLimit::Has),
            (e(true, 5), WeeklyLimit::Has),
        ];
        for (evidence, expected) in cases {
            assert_eq!(detect_weekly_limit(evidence), expected, "{evidence:?}");
        }

        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(WeeklyEvidence::from_history(&names(&["five_hour", "seven_day"]), &[]).weekly_seen);
        assert!(
            !WeeklyEvidence::from_history(&names(&["five_hour", "seven_day_opus"]), &[])
                .weekly_seen,
            "a weekly window on one model is not a weekly limit on the account"
        );
        // Reset times a few seconds apart are one window; five hours apart are two.
        assert_eq!(distinct_windows(&[100, 101, 160]), 1);
        assert_eq!(distinct_windows(&[100, 100 + 5 * 3600, 101]), 2);
        assert_eq!(distinct_windows(&[]), 0);

        assert_eq!(
            weekly_limit(WeeklyLimitSetting::Auto, WeeklyLimit::Unknown),
            WeeklyLimit::Unknown
        );
        assert_eq!(
            weekly_limit(WeeklyLimitSetting::No, WeeklyLimit::Has),
            WeeklyLimit::None
        );
        assert_eq!(
            weekly_limit(WeeklyLimitSetting::Yes, WeeklyLimit::None),
            WeeklyLimit::Has
        );
    }

    #[test]
    fn ranking_keys() {
        let max = cand(
            acct(1, "max", AccountState::Parked, 1),
            WeeklyLimit::Has,
            &[
                ("five_hour", 0.0, NOW + 3600),
                ("seven_day", 0.0, NOW + 86_400),
            ],
        );
        let unknown = cand(
            acct(2, "fresh", AccountState::Parked, 2),
            WeeklyLimit::Unknown,
            &[],
        );
        let team_b = cand(
            acct(3, "team-b", AccountState::Parked, 4),
            WeeklyLimit::None,
            &[("five_hour", 10.0, NOW + 3600)],
        );
        let team_a = cand(
            acct(4, "team-a", AccountState::Parked, 3),
            WeeklyLimit::None,
            &[("five_hour", 60.0, NOW + 3600)],
        );
        let paused = cand(
            acct(5, "paused", AccountState::Paused, 0),
            WeeklyLimit::None,
            &[],
        );
        let pool = vec![max, unknown, team_b, team_a, paused];

        // No weekly limit first, then by order (team-a before team-b despite less room),
        // then unknown, then weekly-limited.
        let order: Vec<i64> = ranking(&pool, POLICY, NOW)
            .iter()
            .map(|r| r.account_id)
            .collect();
        assert_eq!(order, vec![4, 3, 2, 1, 5]);
        assert_eq!(pick(&pool, POLICY, None).unwrap().account.id, 4);
        assert_eq!(pick(&pool, POLICY, Some(4)).unwrap().account.id, 3);

        // With the preference off, the user's order alone decides.
        let off = Policy {
            prefer_no_weekly_limit: false,
            ..POLICY
        };
        assert_eq!(pick(&pool, off, None).unwrap().account.id, 1);

        // Equal places in the order fall back to the most room.
        let tie_a = cand(
            acct(10, "a", AccountState::Parked, 1),
            WeeklyLimit::None,
            &[("five_hour", 70.0, NOW + 3600)],
        );
        let tie_b = cand(
            acct(11, "b", AccountState::Parked, 1),
            WeeklyLimit::None,
            &[("five_hour", 20.0, NOW + 3600)],
        );
        let ties = vec![tie_a, tie_b];
        assert_eq!(pick(&ties, POLICY, None).unwrap().account.id, 11);
        let ranked = ranking(&ties, POLICY, NOW);
        assert!(ranked[0].reason.contains("tied"), "{}", ranked[0].reason);
    }

    #[test]
    fn exhausted_accounts_are_not_eligible_and_say_why() {
        let five = cand(
            acct(1, "a", AccountState::Parked, 1),
            WeeklyLimit::Has,
            &[
                ("five_hour", 99.0, NOW + 7200),
                ("seven_day", 10.0, NOW + 86_400),
            ],
        );
        let week = cand(
            acct(2, "b", AccountState::Parked, 2),
            WeeklyLimit::Has,
            &[
                ("five_hour", 10.0, NOW + 3600),
                ("seven_day", 98.0, NOW + 3 * 86_400),
            ],
        );
        let pool = vec![five, week];
        assert!(pick(&pool, POLICY, None).is_none());
        let ranked = ranking(&pool, POLICY, NOW);
        assert_eq!(
            ranked[0].account_id, 1,
            "the one that frees up first is listed first"
        );
        assert_eq!(ranked[0].reason, "5-hour limit at 99%, resets in 2h 0m");
        assert_eq!(ranked[1].reason, "weekly limit at 98%, resets in 3d 0h");
        assert_eq!(pool[1].blocking().unwrap().name, "seven_day");
        assert_eq!(
            threshold_trigger(&pool[1]).map(|(r, _)| r),
            Some(Reason::Weekly)
        );
        assert_eq!(
            threshold_trigger(&pool[0]).map(|(r, _)| r),
            Some(Reason::FiveHour)
        );
    }

    #[test]
    fn reasons_read_well() {
        let team = cand(
            acct(1, "team", AccountState::Active, 0),
            WeeklyLimit::None,
            &[("five_hour", 20.0, NOW + 3600)],
        );
        let max = cand(
            acct(2, "max", AccountState::Parked, 1),
            WeeklyLimit::Has,
            &[
                ("five_hour", 0.0, NOW + 3600),
                ("seven_day", 41.6, NOW + 86_400),
            ],
        );
        let ranked = ranking(&[team, max], POLICY, NOW);
        assert_eq!(ranked[0].reason, "no weekly limit, 1st in your order");
        assert_eq!(ranked[1].reason, "weekly limit 42% used, 2nd in your order");
        assert_eq!(ordinal(11), "11th");
        assert_eq!(ordinal(22), "22nd");
        assert_eq!(ordinal(103), "103rd");
    }

    #[test]
    fn switch_back_needs_a_strictly_better_rank_and_some_margin() {
        let timing = Timing {
            now_ms: NOW * 1000,
            dwell_ms: 600_000,
            cooldown_ms: 120_000,
            ..Default::default()
        };
        let active = |pos| {
            cand(
                acct(1, "active", AccountState::Active, pos),
                WeeklyLimit::None,
                &[("five_hour", 50.0, NOW + 3600)],
            )
        };
        let other = |pos, pct| {
            cand(
                acct(2, "other", AccountState::Parked, pos),
                WeeklyLimit::None,
                &[("five_hour", pct, NOW + 3600)],
            )
        };
        let d = |pool: &[Candidate], t: Timing| decide(pool, POLICY, t, false, true);

        assert_eq!(
            d(&[active(2), other(1, 0.0)], timing),
            Some(Reason::PreferredAvailable)
        );
        assert_eq!(
            d(&[active(1), other(1, 0.0)], timing),
            None,
            "equal rank never swaps for balance, however much room the other has"
        );
        assert_eq!(d(&[active(1), other(2, 0.0)], timing), None);
        assert_eq!(
            d(&[active(2), other(1, 95.0)], timing),
            None,
            "too close to its threshold to be worth it"
        );
        let early = Timing {
            active_since_ms: timing.now_ms - 599_000,
            ..timing
        };
        assert_eq!(d(&[active(2), other(1, 0.0)], early), None, "dwell");
        let cooling = Timing {
            last_swap_ms: timing.now_ms - 60_000,
            ..timing
        };
        assert_eq!(d(&[active(2), other(1, 0.0)], cooling), None, "cooldown");

        // A weekly-limited account ranks below a no-limit one even when it is first in the
        // order, so the no-limit account is switched back to.
        let max_active = cand(
            acct(1, "max", AccountState::Active, 1),
            WeeklyLimit::Has,
            &[
                ("five_hour", 10.0, NOW + 3600),
                ("seven_day", 10.0, NOW + 86_400),
            ],
        );
        assert_eq!(
            d(&[max_active, other(9, 0.0)], timing),
            Some(Reason::PreferredAvailable)
        );
    }

    #[test]
    fn an_account_found_over_its_limit_on_arrival_is_left_at_once() {
        let now_ms = NOW * 1000;
        let timing = Timing {
            now_ms,
            last_swap_ms: now_ms - 20_000,
            active_since_ms: now_ms - 20_000,
            cooldown_ms: 120_000,
            dwell_ms: 600_000,
            ..Default::default()
        };
        let full = cand(
            acct(1, "full", AccountState::Active, 1),
            WeeklyLimit::None,
            &[("five_hour", 100.0, NOW + 3600)],
        );
        let ready = cand(
            acct(2, "ready", AccountState::Parked, 2),
            WeeklyLimit::None,
            &[("five_hour", 10.0, NOW + 3600)],
        );
        let pool = [full, ready];
        let d = |t: Timing| decide(&pool, POLICY, t, false, true);

        assert_eq!(
            d(timing),
            None,
            "an account that reached its limit in use cools down"
        );
        let arrived = Timing {
            arrived_over_limit: true,
            ..timing
        };
        assert_eq!(d(arrived), Some(Reason::FiveHour));
        assert!(arrived.leaves_at_once(Reason::FiveHour));
        assert!(!arrived.leaves_at_once(Reason::PreferredAvailable));
        assert!(!arrived.leaves_at_once(Reason::Manual));
        assert_eq!(
            d(Timing {
                last_request_ms: now_ms - 5_000,
                ..arrived
            }),
            None,
            "requests from many sessions are still spaced out"
        );
    }

    #[test]
    fn model_families() {
        assert_eq!(model_family("Fable"), Some("fable"));
        assert_eq!(model_family("Opus 4.5"), Some("opus"));
        assert_eq!(model_family("claude-sonnet-5-5"), Some("sonnet"));
        assert_eq!(model_family("Mystery"), None);
    }

    /// A day of status line ticks on a pool of two Team seats (no weekly limit) and one Max
    /// account, with steady use from 08:00 to 22:00. Checks that rotation never ping-pongs:
    /// every switch back goes to a strictly better ranked account whose window has reset since
    /// it was left, and equally ranked accounts never trade places.
    #[test]
    fn a_day_of_ticks_does_not_ping_pong() {
        struct Sim {
            id: i64,
            position: i64,
            weekly: bool,
            five: f64,
            five_resets: Option<i64>,
            week: f64,
            week_resets: i64,
        }
        let start = NOW - NOW % 86_400;
        let mut sims = [
            Sim {
                id: 1,
                position: 1,
                weekly: false,
                five: 0.0,
                five_resets: None,
                week: 0.0,
                week_resets: 0,
            },
            Sim {
                id: 2,
                position: 2,
                weekly: false,
                five: 0.0,
                five_resets: None,
                week: 0.0,
                week_resets: 0,
            },
            Sim {
                id: 3,
                position: 3,
                weekly: true,
                five: 0.0,
                five_resets: None,
                week: 30.0,
                week_resets: start + 4 * 86_400,
            },
        ];
        let mut active = 1i64;
        let mut timing = Timing {
            cooldown_ms: 120_000,
            dwell_ms: 600_000,
            ..Default::default()
        };
        // (time, from, to, reason)
        let mut swaps: Vec<(i64, i64, i64, Reason)> = Vec::new();
        // When each account was last left, and its 5-hour reset time then.
        let mut left: std::collections::HashMap<i64, Option<i64>> = Default::default();

        let mut now = start;
        while now < start + 86_400 {
            let hour = (now - start) / 3600;
            let busy = (8..22).contains(&hour);
            for s in sims.iter_mut() {
                if s.five_resets.is_some_and(|r| r <= now) {
                    s.five = 0.0;
                    s.five_resets = None;
                }
                if s.id == active && busy {
                    // About 0.45 points a minute: a 5-hour window fills in under four hours.
                    if s.five_resets.is_none() {
                        s.five_resets = Some(now + 5 * 3600);
                    }
                    s.five += 0.225;
                    if s.weekly {
                        s.week += 0.02;
                    }
                }
            }
            let pool: Vec<Candidate> = sims
                .iter()
                .map(|s| {
                    let state = if s.id == active {
                        AccountState::Active
                    } else {
                        AccountState::Parked
                    };
                    let mut windows = Vec::new();
                    if let Some(r) = s.five_resets {
                        windows.push(("five_hour", s.five, r));
                    }
                    if s.weekly {
                        windows.push(("seven_day", s.week, s.week_resets));
                    }
                    let mut c = cand(
                        acct(s.id, &format!("acct-{}", s.id), state, s.position),
                        if s.weekly {
                            WeeklyLimit::Has
                        } else {
                            WeeklyLimit::None
                        },
                        &windows,
                    );
                    c.windows = window_usage(Some(&sample(&windows)), now, T, None);
                    c
                })
                .collect();

            timing.now_ms = now * 1000;
            if let Some(reason) = decide(&pool, POLICY, timing, false, true) {
                timing.last_request_ms = timing.now_ms;
                let target = match reason {
                    Reason::PreferredAvailable => switch_back_target(&pool, POLICY),
                    _ => pick(&pool, POLICY, Some(active)),
                };
                if let Some(target) = target {
                    let from = pool.iter().find(|c| c.account.id == active).unwrap();
                    if reason == Reason::PreferredAvailable {
                        assert!(
                            POLICY.rank(target) < POLICY.rank(from),
                            "switch back only to a strictly better rank"
                        );
                    }
                    // Leaving an account at its limit: it must not come back before it reset.
                    // Leaving it to switch back elsewhere: it is still fine to return to.
                    let reset_then = sims.iter().find(|s| s.id == active).unwrap().five_resets;
                    match reason {
                        Reason::PreferredAvailable => left.remove(&active),
                        _ => left.insert(active, reset_then),
                    };
                    if let Some(Some(reset_then)) = left.get(&target.account.id) {
                        assert!(
                            now >= *reset_then,
                            "returned to account {} before its window reset",
                            target.account.id
                        );
                    }
                    swaps.push((now, active, target.account.id, reason));
                    active = target.account.id;
                    timing.last_swap_ms = timing.now_ms;
                    timing.active_since_ms = timing.now_ms;
                }
            }
            now += 30;
        }

        assert!(!swaps.is_empty(), "the day needed some rotation");
        for pair in swaps.windows(2) {
            let ((t1, a, b, _), (t2, c, d, _)) = (pair[0], pair[1]);
            assert_eq!(b, c);
            if d == a {
                assert!(
                    t2 - t1 >= 600,
                    "{a} -> {b} -> {a} within {}s: ping-pong",
                    t2 - t1
                );
            }
        }
        // The two seats carry the day, so the weekly-limited account sees little use, and the
        // first seat is preferred again as soon as it is usable.
        assert!(swaps.iter().any(|s| s.3 == Reason::PreferredAvailable));
        assert!(swaps.len() <= 8, "too many swaps: {swaps:?}");
    }
}
