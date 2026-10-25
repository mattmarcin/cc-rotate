//! When to swap, and which account to swap to. Pure functions over store data.
//!
//! Limits are modelled generically. A sample holds every usage window the data source reported
//! (Claude Code's status line input: `five_hour`, `seven_day`, and any window it adds later),
//! and `classify` says what each one limits: a 5-hour session window or a weekly window, on all
//! models or on one model family. An account is ineligible while any window that applies to the
//! model in use is at or above its threshold.
//!
//! Ranking, best first (`Policy::tier`):
//! 1. With `prefer_no_weekly_limit` on: accounts without a weekly limit.
//! 2. Accounts whose weekly budget would otherwise reset unused (`budget::expiring`), the
//!    soonest reset first.
//! 3. With `prefer_no_weekly_limit` on: accounts whose weekly limit is not known yet, then
//!    accounts with one.
//! 4. Within a tier (`Policy::place`): the account in use, then accounts whose 5-hour window is
//!    already running with `RUNNING_MARGIN` points to spare, the soonest reset first, then the
//!    user's order (`position`), then running 5-hour windows closer to their limit. With
//!    `strict_order` on, the user's order alone.
//! 5. The most room before a threshold, then the label.
//!
//! The ranking only orders picks. The account in use is kept until a limit, since what is left
//! in a 5-hour window is lost when it resets, so leaving a running window for a fresh one only
//! adds swaps. The one exception is `switch_back_target`: an account that may have a weekly
//! limit gives way to one without as soon as one is ready. An account's weekly cap
//! (`Account::weekly_cap`) lowers its weekly thresholds.

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

/// An account without a weekly limit is only switched back to while every window that applies
/// is at least this many points below its threshold, so it is not left again a moment later.
pub const SWITCH_BACK_MARGIN: f64 = 5.0;

/// A running 5-hour window goes before fresh accounts only while it is at least this many
/// points below its threshold. With less, it would last minutes: another swap soon after for a
/// few points. Only the 5-hour window counts: what is left of it is what a reset loses.
pub const RUNNING_MARGIN: f64 = 10.0;

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
    /// The account's weekly cap, when lower than `weekly`: it applies to weekly windows only.
    pub weekly_cap: Option<f64>,
}

impl Thresholds {
    /// The thresholds for one account: its weekly cap, when lower than the weekly threshold,
    /// takes its place.
    pub fn for_account(&self, account: &Account) -> Thresholds {
        Thresholds {
            weekly_cap: account.weekly_cap,
            ..*self
        }
    }

    /// Windows of an unknown kind get the stricter of the two thresholds.
    pub fn for_kind(&self, kind: WindowKind) -> f64 {
        match kind {
            WindowKind::Session => self.session,
            WindowKind::Weekly => self.weekly_cap.map_or(self.weekly, |c| c.min(self.weekly)),
            // A cap keeps part of the week for the user; it is no margin for unknown windows.
            WindowKind::Other => self.session.min(self.weekly),
        }
    }
}

impl From<&Settings> for Thresholds {
    fn from(s: &Settings) -> Self {
        Self {
            session: s.swap_threshold_5h,
            weekly: s.swap_threshold_7d,
            weekly_cap: None,
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
    /// An account without a weekly limit is usable while one with a weekly limit is in use
    /// (switch back, `switch_back_target`).
    PreferredAvailable,
    StopFailure,
    DeadLogin,
    Manual,
    Login,
    /// Every account is at a limit and the one in use runs on paid extra usage: move to one
    /// without, so sessions wait instead of paying (`avoid_extra_usage`).
    AvoidExtraUsage,
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
            Self::AvoidExtraUsage => "avoid_extra_usage",
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
            "avoid_extra_usage" => Self::AvoidExtraUsage,
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
    /// Its login is not used until a usage check settles whether it still works
    /// (`usage_check::CheckState::held_back`).
    pub held_back: bool,
    /// Its weekly budget would reset unused unless it is used now (`budget::expiring`).
    pub expiring: Option<crate::budget::Expiring>,
    /// Until when (epoch seconds) its room is not trusted: a turn stopped on it at a usage limit
    /// that its readings did not show, and no different reading has come since
    /// (`swap::stop_hold_until`). Until then it is not switched back to, and it is picked last.
    pub stop_hold_until: Option<i64>,
}

impl Candidate {
    /// Its room is not trusted at `now_s` (`stop_hold_until`).
    pub fn on_stop_hold(&self, now_s: i64) -> bool {
        self.stop_hold_until.is_some_and(|until| now_s < until)
    }

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

    /// When the account's running 5-hour window resets, if one is running at `now_s`. An
    /// account without one starts a new window with its next request.
    pub fn session_reset(&self, now_s: i64) -> Option<i64> {
        self.five_hour()
            .and_then(|w| w.resets_at)
            .filter(|&r| r > now_s)
    }

    /// Whether a 5-hour window is running at `now_s`: one that resets later, or one in use whose
    /// reading gave no reset time.
    pub fn session_running(&self, now_s: i64) -> bool {
        self.session_reset(now_s).is_some()
            || self
                .five_hour()
                .is_some_and(|w| w.resets_at.is_none() && w.pct > 0.0)
    }

    pub fn usable(&self) -> bool {
        matches!(
            self.account.state,
            AccountState::Active | AccountState::Parked
        )
    }

    /// Parked and below every threshold that applies: the scheduler may pick it.
    pub fn eligible(&self) -> bool {
        self.account.state == AccountState::Parked && self.blocking().is_none() && !self.held_back
    }

    pub fn has_margin(&self) -> bool {
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
    /// Accounts whose weekly budget would reset unused come first (`Candidate::expiring`).
    pub use_expiring_weekly: bool,
    /// Pick by the user's order alone within a tier, instead of keeping the account in use and
    /// using running 5-hour windows first.
    pub strict_order: bool,
}

impl From<&Settings> for Policy {
    fn from(s: &Settings) -> Self {
        Self {
            thresholds: Thresholds::from(s),
            prefer_no_weekly_limit: s.prefer_no_weekly_limit,
            use_expiring_weekly: s.use_expiring_weekly,
            strict_order: s.strict_order,
        }
    }
}

impl Policy {
    /// The account's weekly budget would reset unused unless it is used now, and that counts.
    pub fn expiring<'a>(&self, c: &'a Candidate) -> Option<&'a crate::budget::Expiring> {
        c.expiring.as_ref().filter(|_| self.use_expiring_weekly)
    }

    /// 0 for no weekly limit, which always goes first, even before a weekly budget that would
    /// reset unused; then 1 for an account whose weekly budget would reset unused, 2 for not
    /// known yet, 3 for a weekly limit. With the preference off: 1 for a weekly budget that would
    /// reset unused, 2 for every other account.
    pub fn tier(&self, c: &Candidate) -> u8 {
        let expiring = self.expiring(c).is_some();
        if !self.prefer_no_weekly_limit {
            return if expiring { 1 } else { 2 };
        }
        match c.weekly_limit {
            WeeklyLimit::None => 0,
            _ if expiring => 1,
            WeeklyLimit::Unknown => 2,
            WeeklyLimit::Has => 3,
        }
    }

    /// The rank of an account in the pick order: lower is better. Room is deliberately not part
    /// of it. Accounts whose weekly budget would reset unused go by their reset, the soonest
    /// first, since only one account is in use at a time. The user's order is part of it only
    /// with `strict_order`.
    pub fn rank(&self, c: &Candidate) -> (u8, i64) {
        let tier = self.tier(c);
        match self.expiring(c) {
            Some(e) if tier == 1 => (tier, e.resets_at),
            _ if self.strict_order => (tier, c.account.position),
            _ => (tier, 0),
        }
    }

    /// Where an account goes among accounts of the same rank, best first. The account in use
    /// comes first: it is kept until a limit. Then accounts whose 5-hour window is already
    /// running with `RUNNING_MARGIN` to spare, the soonest reset first, since what is left in a
    /// window is lost when it resets (a running window without a known reset time goes last of
    /// these). Then accounts with no window running, in the user's order. Last, running windows
    /// close to their limit, which would only last minutes, the most room first (the tie-break
    /// after `place`). With `strict_order` the rank already holds the order, and this changes
    /// nothing.
    pub fn place(&self, c: &Candidate, now_s: i64) -> (u8, i64) {
        if self.strict_order || c.account.state == AccountState::Active {
            return (0, 0);
        }
        if !c.session_running(now_s) {
            return (2, c.account.position);
        }
        let room = c
            .five_hour()
            .map_or(self.thresholds.session, WindowUsage::room);
        if room >= RUNNING_MARGIN {
            (1, c.session_reset(now_s).unwrap_or(i64::MAX))
        } else {
            (3, 0)
        }
    }

    fn pick_order(&self, a: &Candidate, b: &Candidate, now_s: i64) -> Ordering {
        a.on_stop_hold(now_s)
            .cmp(&b.on_stop_hold(now_s))
            .then(self.rank(a).cmp(&self.rank(b)))
            .then(self.place(a, now_s).cmp(&self.place(b, now_s)))
            .then(b.room(self.thresholds).total_cmp(&a.room(self.thresholds)))
            .then_with(|| a.account.label.cmp(&b.account.label))
    }
}

/// The best eligible account at `now_s`, excluding `exclude`.
pub fn pick(
    candidates: &[Candidate],
    policy: Policy,
    exclude: Option<i64>,
    now_s: i64,
) -> Option<&Candidate> {
    candidates
        .iter()
        .filter(|c| c.eligible() && Some(c.account.id) != exclude)
        .min_by(|a, b| policy.pick_order(a, b, now_s))
}

/// The account to switch back to, the only switch back there is: while an account that may have
/// a weekly limit is in use, the best eligible account without one that has
/// `SWITCH_BACK_MARGIN` to spare, so weekly budget is only spent while no such account can be
/// used. Not to an account a turn just stopped on at a limit its readings do not show
/// (`Candidate::on_stop_hold`), which would swap back and forth every cooldown. Never with
/// `prefer_no_weekly_limit` off, and `None` while the active account is over a threshold, since
/// the threshold trigger handles that. The account in use is otherwise kept until a limit.
pub fn switch_back_target(
    candidates: &[Candidate],
    policy: Policy,
    now_s: i64,
) -> Option<&Candidate> {
    let active = candidates
        .iter()
        .find(|c| c.account.state == AccountState::Active)?;
    if !policy.prefer_no_weekly_limit
        || active.weekly_limit == WeeklyLimit::None
        || active.blocking().is_some()
    {
        return None;
    }
    candidates
        .iter()
        .filter(|c| {
            c.eligible()
                && c.weekly_limit == WeeklyLimit::None
                && c.has_margin()
                && !c.on_stop_hold(now_s)
        })
        .min_by(|a, b| policy.pick_order(a, b, now_s))
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
    pub last_request_ms: i64,
    pub cooldown_ms: i64,
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
    if timing.throttled(reason) {
        return None;
    }
    switch_back_target(candidates, policy, timing.now_ms / 1000).map(|_| reason)
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
        .filter(|c| c.usable() && c.blocking().is_none() && !c.held_back)
        .collect();
    open.sort_by(|a, b| policy.pick_order(a, b, now_s));
    let mut blocked: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.usable() && (c.blocking().is_some() || c.held_back))
        .collect();
    blocked.sort_by_key(|c| (c.blocked_until().unwrap_or(i64::MAX), c.order));
    let rest = candidates.iter().filter(|c| !c.usable());

    let mut out = Vec::with_capacity(candidates.len());
    for (i, c) in open.iter().enumerate() {
        let key = |x: &Candidate| (policy.rank(x), policy.place(x, now_s));
        let tied = open
            .iter()
            .any(|o| o.account.id != c.account.id && key(o) == key(c));
        out.push(Ranked {
            account_id: c.account.id,
            label: c.account.label.clone(),
            rank: Some(i + 1),
            eligible: c.eligible(),
            reason: open_reason(c, policy, tied, now_s),
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

fn open_reason(c: &Candidate, policy: Policy, tied: bool, now_s: i64) -> String {
    if let Some(until) = c.stop_hold_until.filter(|&u| now_s < u) {
        return format!(
            "a session stopped on it at a limit its usage didn't show, so it goes last for {} or until a new reading",
            time::span(until - now_s)
        );
    }
    if let Some(e) = policy.expiring(c) {
        return format!(
            "weekly budget expires in {} with {:.0}% left",
            time::span(e.resets_at - now_s),
            e.left
        );
    }
    let mut parts: Vec<String> = Vec::new();
    if policy.prefer_no_weekly_limit {
        parts.push(match (c.weekly_limit, c.weekly()) {
            (WeeklyLimit::None, _) => "no weekly limit".into(),
            (WeeklyLimit::Unknown, _) => "weekly limit not known yet".into(),
            (WeeklyLimit::Has, Some(w)) => format!("weekly limit {:.0}% used", w.pct),
            (WeeklyLimit::Has, None) => "has a weekly limit".into(),
        });
    }
    let in_order = || format!("{} in your order", ordinal(c.order + 1));
    parts.push(match policy.place(c, now_s) {
        _ if policy.strict_order => in_order(),
        (0, _) => "in use".into(),
        (1 | 3, _) => {
            let mut text = match policy.place(c, now_s).0 {
                1 => "5-hour window already running".to_string(),
                _ => "5-hour window close to its limit".to_string(),
            };
            if let Some(reset) = c.session_reset(now_s) {
                text.push_str(&format!(", resets in {}", time::span(reset - now_s)));
            }
            text
        }
        _ => in_order(),
    });
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
        AccountState::NeedsLogin => return "signed out".into(),
        _ => {}
    }
    if c.held_back && c.blocking().is_none() {
        return "on hold until a check confirms its login".into();
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
        weekly_cap: None,
    };
    const POLICY: Policy = Policy {
        thresholds: T,
        prefer_no_weekly_limit: true,
        use_expiring_weekly: true,
        strict_order: false,
    };
    const STRICT: Policy = Policy {
        strict_order: true,
        ..POLICY
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
            weekly_cap: None,
            use_expiring: true,
            keep_running: false,
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
            held_back: false,
            expiring: None,
            stop_hold_until: None,
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
            weekly_cap: None,
        };
        assert_eq!(mixed.for_kind(WindowKind::Session), 90.0);
        assert_eq!(mixed.for_kind(WindowKind::Weekly), 95.0);
        assert_eq!(mixed.for_kind(WindowKind::Other), 90.0);
        // A weekly cap lowers weekly windows only.
        let capped = Thresholds {
            weekly_cap: Some(50.0),
            ..mixed
        };
        assert_eq!(capped.for_kind(WindowKind::Weekly), 50.0);
        assert_eq!(capped.for_kind(WindowKind::Other), 90.0);
        assert_eq!(capped.for_kind(WindowKind::Session), 90.0);

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

        // With strict_order: no weekly limit first, then by order (team-a before team-b
        // despite less room), then unknown, then weekly-limited.
        let order = |policy| -> Vec<i64> {
            ranking(&pool, policy, NOW)
                .iter()
                .map(|r| r.account_id)
                .collect()
        };
        assert_eq!(order(STRICT), vec![4, 3, 2, 1, 5]);
        assert_eq!(pick(&pool, STRICT, None, NOW).unwrap().account.id, 4);
        assert_eq!(pick(&pool, STRICT, Some(4), NOW).unwrap().account.id, 3);
        // Without it, the two seats' windows reset together, so the one with more room goes
        // first; the tiers are the same.
        assert_eq!(order(POLICY), vec![3, 4, 2, 1, 5]);

        // With the preference off, only running windows and the user's order decide.
        let off = Policy {
            prefer_no_weekly_limit: false,
            ..STRICT
        };
        assert_eq!(pick(&pool, off, None, NOW).unwrap().account.id, 1);

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
        for policy in [POLICY, STRICT] {
            assert_eq!(pick(&ties, policy, None, NOW).unwrap().account.id, 11);
            let ranked = ranking(&ties, policy, NOW);
            assert!(ranked[0].reason.contains("tied"), "{}", ranked[0].reason);
        }
    }

    /// Within a tier: the account in use, then running 5-hour windows with room, the soonest
    /// reset first whatever the order, then fresh accounts in the user's order, then running
    /// windows close to a limit.
    #[test]
    fn running_windows_go_first_the_soonest_reset_first() {
        let seat = |id, state, position, windows: &[(&str, f64, i64)]| {
            cand(
                acct(id, &format!("s{id}"), state, position),
                WeeklyLimit::None,
                windows,
            )
        };
        let pool = vec![
            seat(1, AccountState::Parked, 0, &[]),
            // A window that reset an hour ago: fresh again.
            seat(
                2,
                AccountState::Parked,
                1,
                &[("five_hour", 70.0, NOW - 3600)],
            ),
            seat(
                3,
                AccountState::Parked,
                2,
                &[("five_hour", 40.0, NOW + 4 * 3600)],
            ),
            seat(
                4,
                AccountState::Parked,
                3,
                &[("five_hour", 80.0, NOW + 3600)],
            ),
            seat(
                5,
                AccountState::Parked,
                4,
                // 8 points from its threshold: close to a limit, after the fresh accounts.
                &[("five_hour", 90.0, NOW + 600)],
            ),
            seat(
                6,
                AccountState::Active,
                5,
                &[("five_hour", 50.0, NOW + 2 * 3600)],
            ),
            seat(
                7,
                AccountState::Parked,
                6,
                // At its limit: blocked, so after every account that can be used.
                &[("five_hour", 99.0, NOW + 1800)],
            ),
        ];
        let order: Vec<i64> = ranking(&pool, POLICY, NOW)
            .iter()
            .map(|r| r.account_id)
            .collect();
        assert_eq!(order, vec![6, 4, 3, 1, 2, 5, 7]);
        assert_eq!(pick(&pool, POLICY, Some(6), NOW).unwrap().account.id, 4);
        // Once 4's window has reset, 3's is the soonest to reset.
        assert_eq!(
            pick(&pool, POLICY, Some(6), NOW + 3600).unwrap().account.id,
            3
        );

        let ranked = ranking(&pool, POLICY, NOW);
        let reason = |id| &ranked.iter().find(|r| r.account_id == id).unwrap().reason;
        assert_eq!(reason(6), "no weekly limit, in use");
        assert_eq!(
            reason(4),
            "no weekly limit, 5-hour window already running, resets in 1h 0m"
        );
        assert_eq!(reason(1), "no weekly limit, 1st in your order");
        assert_eq!(reason(2), "no weekly limit, 2nd in your order");
        assert_eq!(
            reason(5),
            "no weekly limit, 5-hour window close to its limit, resets in 10m"
        );

        // A better tier still comes first, running window or not.
        let mut tiers = pool.clone();
        tiers[0].weekly_limit = WeeklyLimit::None;
        for c in &mut tiers[1..] {
            c.weekly_limit = WeeklyLimit::Has;
        }
        assert_eq!(pick(&tiers, POLICY, Some(6), NOW).unwrap().account.id, 1);
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
        assert!(pick(&pool, POLICY, None, NOW).is_none());
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
        let ranked = ranking(&[team.clone(), max.clone()], STRICT, NOW);
        assert_eq!(ranked[0].reason, "no weekly limit, 1st in your order");
        assert_eq!(ranked[1].reason, "weekly limit 42% used, 2nd in your order");
        let ranked = ranking(&[team, max], POLICY, NOW);
        assert_eq!(ranked[0].reason, "no weekly limit, in use");
        assert_eq!(
            ranked[1].reason,
            "weekly limit 42% used, 5-hour window already running, resets in 1h 0m"
        );
        assert_eq!(ordinal(11), "11th");
        assert_eq!(ordinal(22), "22nd");
        assert_eq!(ordinal(103), "103rd");
    }

    #[test]
    fn the_only_switch_back_is_to_an_account_without_a_weekly_limit() {
        let timing = Timing {
            now_ms: NOW * 1000,
            cooldown_ms: 120_000,
            ..Default::default()
        };
        let active = |pos, weekly| {
            cand(
                acct(1, "active", AccountState::Active, pos),
                weekly,
                &[("five_hour", 50.0, NOW + 3600)],
            )
        };
        let other = |pos, pct, weekly| {
            cand(
                acct(2, "other", AccountState::Parked, pos),
                weekly,
                &[("five_hour", pct, NOW + 3600)],
            )
        };
        let none = WeeklyLimit::None;
        for policy in [POLICY, STRICT] {
            let d = |pool: &[Candidate], t: Timing| decide(pool, policy, t, false, true);
            // Within a tier the account in use is kept, however high the other is in the order
            // and however much room it has.
            assert_eq!(d(&[active(2, none), other(1, 0.0, none)], timing), None);
            assert_eq!(
                d(
                    &[active(2, WeeklyLimit::Has), other(1, 0.0, WeeklyLimit::Has)],
                    timing
                ),
                None
            );
            // An account with a weekly limit gives way to one without as soon as it is ready,
            // with no minimum time in use, even when it is first in the order.
            for weekly in [WeeklyLimit::Has, WeeklyLimit::Unknown] {
                assert_eq!(
                    d(&[active(1, weekly), other(9, 0.0, none)], timing),
                    Some(Reason::PreferredAvailable)
                );
            }
            assert_eq!(
                d(&[active(1, WeeklyLimit::Has), other(9, 95.0, none)], timing),
                None,
                "too close to its threshold to be worth it"
            );
            let mut stopped = other(9, 0.0, none);
            stopped.stop_hold_until = Some(NOW + 60);
            assert_eq!(
                d(&[active(1, WeeklyLimit::Has), stopped.clone()], timing),
                None,
                "a turn just stopped on it at a limit its readings do not show"
            );
            let later = Timing {
                now_ms: (NOW + 60) * 1000,
                ..timing
            };
            assert_eq!(
                d(&[active(1, WeeklyLimit::Has), stopped], later),
                Some(Reason::PreferredAvailable),
                "the hold is over"
            );
            let cooling = Timing {
                last_swap_ms: timing.now_ms - 60_000,
                ..timing
            };
            assert_eq!(
                d(&[active(1, WeeklyLimit::Has), other(9, 0.0, none)], cooling),
                None,
                "cooldown"
            );
        }
        // Without the preference there is no switch back at all.
        let off = Policy {
            prefer_no_weekly_limit: false,
            ..POLICY
        };
        assert_eq!(
            decide(
                &[active(1, WeeklyLimit::Has), other(9, 0.0, none)],
                off,
                timing,
                false,
                true
            ),
            None
        );
    }

    #[test]
    fn no_weekly_limit_goes_before_a_weekly_budget_that_would_reset_unused() {
        let mut expiring = cand(
            acct(1, "max", AccountState::Parked, 1),
            WeeklyLimit::Has,
            &[("seven_day", 20.0, NOW + 86_400)],
        );
        expiring.expiring = Some(crate::budget::Expiring {
            resets_at: NOW + 86_400,
            left: 80.0,
            hours_needed: 30.0,
            hours_available: 20.0,
        });
        let seat = cand(
            acct(2, "seat", AccountState::Parked, 2),
            WeeklyLimit::None,
            &[("five_hour", 10.0, NOW + 3600)],
        );
        let plain = cand(
            acct(3, "alt", AccountState::Parked, 0),
            WeeklyLimit::Has,
            &[("seven_day", 10.0, NOW + 86_400)],
        );
        let pool = vec![expiring.clone(), seat.clone(), plain.clone()];
        assert_eq!(pick(&pool, POLICY, None, NOW).unwrap().account.id, 2);
        // Among accounts with a weekly limit, the budget that would reset unused goes first.
        let pool = vec![expiring.clone(), plain.clone()];
        assert_eq!(pick(&pool, POLICY, None, NOW).unwrap().account.id, 1);

        // Without the preference, the budget that would reset unused goes first of all.
        let off = Policy {
            prefer_no_weekly_limit: false,
            ..POLICY
        };
        let pool = vec![expiring.clone(), seat.clone(), plain.clone()];
        assert_eq!(pick(&pool, off, None, NOW).unwrap().account.id, 1);

        // Spending an expiring budget, the account in use still gives way to a ready seat.
        let mut in_use = expiring;
        in_use.account.state = AccountState::Active;
        let timing = Timing {
            now_ms: NOW * 1000,
            cooldown_ms: 120_000,
            ..Default::default()
        };
        assert_eq!(
            decide(&[in_use, seat], POLICY, timing, false, true),
            Some(Reason::PreferredAvailable)
        );
    }

    #[test]
    fn an_account_found_over_its_limit_on_arrival_is_left_at_once() {
        let now_ms = NOW * 1000;
        let timing = Timing {
            now_ms,
            last_swap_ms: now_ms - 20_000,
            cooldown_ms: 120_000,
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

    /// Only the 5-hour window decides whether a running window goes before fresh accounts: a
    /// weekly window near its cap leaves a nearly unused 5-hour window first. A reading with
    /// usage but no reset time is a running window too, after those with one, and windows
    /// close to their limit go by the most room.
    #[test]
    fn the_running_margin_is_about_the_five_hour_window() {
        let max = |id, position, windows: &[(&str, f64, i64)]| {
            let mut c = cand(
                acct(id, &format!("m{id}"), AccountState::Parked, position),
                WeeklyLimit::Has,
                windows,
            );
            c.account.weekly_cap = Some(50.0);
            c.windows = window_usage(
                Some(&sample(windows)),
                NOW,
                T.for_account(&c.account),
                Some("claude-opus-4-1"),
            );
            c
        };
        let pool = vec![
            max(1, 0, &[]),
            max(
                2,
                1,
                &[
                    ("five_hour", 5.0, NOW + 3600),
                    ("seven_day", 42.0, NOW + 86_400),
                ],
            ),
        ];
        assert_eq!(POLICY.place(&pool[1], NOW), (1, NOW + 3600));
        assert_eq!(pick(&pool, POLICY, None, NOW).unwrap().account.id, 2);

        let mut unknown_reset = cand(
            acct(3, "u", AccountState::Parked, 0),
            WeeklyLimit::None,
            &[],
        );
        unknown_reset.windows = window_usage(
            Some(&Sample {
                windows: vec![SampleWindow {
                    name: "five_hour".into(),
                    pct: 40.0,
                    resets_at: None,
                }],
                ..sample(&[])
            }),
            NOW,
            T,
            None,
        );
        let known = cand(
            acct(4, "k", AccountState::Parked, 1),
            WeeklyLimit::None,
            &[("five_hour", 60.0, NOW + 4 * 3600)],
        );
        let fresh = cand(
            acct(5, "f", AccountState::Parked, 2),
            WeeklyLimit::None,
            &[],
        );
        let pool = vec![unknown_reset, known, fresh];
        let order: Vec<i64> = ranking(&pool, POLICY, NOW)
            .iter()
            .map(|r| r.account_id)
            .collect();
        assert_eq!(order, vec![4, 3, 5]);
        assert_eq!(
            ranking(&pool, POLICY, NOW)[1].reason,
            "no weekly limit, 5-hour window already running"
        );

        let near = |id, pct, resets_in| {
            cand(
                acct(id, &format!("n{id}"), AccountState::Parked, 0),
                WeeklyLimit::None,
                &[("five_hour", pct, NOW + resets_in)],
            )
        };
        // 1 point left resetting in 5 minutes, 9 points left resetting in 4 hours.
        let pool = vec![near(6, 97.0, 300), near(7, 89.0, 4 * 3600)];
        assert_eq!(pick(&pool, POLICY, None, NOW).unwrap().account.id, 7);
    }

    /// A day of status line ticks on a pool of two Team seats (no weekly limit) and one Max
    /// account, with steady use from 08:00 to 22:00. Checks that rotation never ping-pongs: the
    /// account in use is kept until a limit, the only switch back is from the Max account to a
    /// seat that is ready again, and no account is returned to before its window reset.
    #[test]
    fn a_day_of_ticks_does_not_ping_pong() {
        for policy in [POLICY, STRICT] {
            let swaps = day_of_ticks(policy, 0.225, 8);
            for s in &swaps {
                if s.3 == Reason::PreferredAvailable {
                    assert_eq!(s.1, 3, "only the Max account swaps back: {swaps:?}");
                } else {
                    assert_eq!(s.3, Reason::FiveHour, "{swaps:?}");
                }
            }
            // The two seats carry the day between them, each fresh again by the time the other
            // is at its limit: the Max account's weekly budget is never needed.
            assert!(swaps.iter().all(|s| s.2 != 3), "{swaps:?}");
        }
    }

    /// A busier day: both seats reach their limits before either resets, so the Max account
    /// carries the gaps, and gives way to a seat as soon as one has reset.
    #[test]
    fn a_busy_day_switches_back_to_the_seats_without_ping_pong() {
        for policy in [POLICY, STRICT] {
            let swaps = day_of_ticks(policy, 0.45, 16);
            let backs: Vec<_> = swaps
                .iter()
                .filter(|s| s.3 == Reason::PreferredAvailable)
                .collect();
            assert!(!backs.is_empty(), "{swaps:?}");
            for s in &swaps {
                if s.3 == Reason::PreferredAvailable {
                    assert!(s.1 == 3 && s.2 != 3, "only from the Max account: {swaps:?}");
                } else {
                    assert_eq!(s.3, Reason::FiveHour, "{swaps:?}");
                }
                if s.2 == 3 {
                    assert_eq!(s.3, Reason::FiveHour, "the Max account only at a limit");
                }
            }
        }
    }

    /// `per_tick`: 5-hour points the account in use spends every 30 s of the busy hours.
    fn day_of_ticks(
        policy: Policy,
        per_tick: f64,
        max_swaps: usize,
    ) -> Vec<(i64, i64, i64, Reason)> {
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
                    // At 0.225 a tick, a 5-hour window fills in under four hours.
                    if s.five_resets.is_none() {
                        s.five_resets = Some(now + 5 * 3600);
                    }
                    s.five += per_tick;
                    if s.weekly {
                        s.week += per_tick * 0.09;
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
            if let Some(reason) = decide(&pool, policy, timing, false, true) {
                timing.last_request_ms = timing.now_ms;
                let target = match reason {
                    Reason::PreferredAvailable => switch_back_target(&pool, policy, now),
                    _ => pick(&pool, policy, Some(active), now),
                };
                if let Some(target) = target {
                    if reason == Reason::PreferredAvailable {
                        assert_eq!(
                            target.weekly_limit,
                            WeeklyLimit::None,
                            "switch back only to an account without a weekly limit"
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
                    t2 - t1 >= 120,
                    "{a} -> {b} -> {a} within the cooldown ({}s)",
                    t2 - t1
                );
            }
        }
        assert!(swaps.len() <= max_swaps, "too many swaps: {swaps:?}");
        swaps
    }
}
