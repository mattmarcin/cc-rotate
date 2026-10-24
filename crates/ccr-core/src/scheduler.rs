//! When to swap, and which account to swap to. Pure functions over store data.

use serde::Serialize;

use crate::store::{Account, AccountState, Sample, Settings};

#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub five_hour: f64,
    pub weekly: f64,
}

impl From<&Settings> for Thresholds {
    fn from(s: &Settings) -> Self {
        Self {
            five_hour: s.swap_threshold_5h,
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

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Usage {
    pub five_hour: f64,
    pub weekly: f64,
}

impl Usage {
    pub fn from_sample(sample: Option<&Sample>, now_s: i64) -> Self {
        match sample {
            Some(s) => Self {
                five_hour: effective(s.five_pct, s.five_resets_at, now_s),
                weekly: effective(s.week_pct, s.week_resets_at, now_s),
            },
            None => Self {
                five_hour: 0.0,
                weekly: 0.0,
            },
        }
    }

    pub fn below(&self, t: Thresholds) -> bool {
        self.five_hour < t.five_hour && self.weekly < t.weekly
    }
}

/// Why a swap is wanted. The string form is what events record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    FiveHour,
    Weekly,
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
            "stop_failure" => Self::StopFailure,
            "dead_login" => Self::DeadLogin,
            "manual" => Self::Manual,
            "login" => Self::Login,
            _ => return None,
        })
    }

    /// Threshold triggers respect the cooldown; the others are urgent or explicit.
    pub fn respects_cooldown(self) -> bool {
        matches!(self, Self::FiveHour | Self::Weekly)
    }
}

/// The threshold trigger for the active account's live usage, if any.
pub fn threshold_trigger(usage: Usage, t: Thresholds) -> Option<Reason> {
    if usage.weekly >= t.weekly {
        Some(Reason::Weekly)
    } else if usage.five_hour >= t.five_hour {
        Some(Reason::FiveHour)
    } else {
        None
    }
}

#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    pub account: &'a Account,
    pub usage: Usage,
}

/// The best parked account below both thresholds: lowest weekly usage first so the weekly
/// budget drains evenly, then lowest 5-hour usage, then priority, then label.
pub fn pick<'a>(
    candidates: &[Candidate<'a>],
    t: Thresholds,
    exclude: Option<i64>,
) -> Option<&'a Account> {
    candidates
        .iter()
        .filter(|c| c.account.state == AccountState::Parked)
        .filter(|c| Some(c.account.id) != exclude)
        .filter(|c| c.usage.below(t))
        .min_by(|a, b| {
            a.usage
                .weekly
                .total_cmp(&b.usage.weekly)
                .then(a.usage.five_hour.total_cmp(&b.usage.five_hour))
                .then(a.account.priority.cmp(&b.account.priority))
                .then(a.account.label.cmp(&b.account.label))
        })
        .map(|c| c.account)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(id: i64, label: &str, state: AccountState, priority: i64) -> Account {
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
            priority,
            added_at: 0,
            last_login_at: None,
        }
    }

    const T: Thresholds = Thresholds {
        five_hour: 98.0,
        weekly: 98.0,
    };

    #[test]
    fn effective_resets_to_zero() {
        assert_eq!(effective(Some(80.0), Some(100), 100), 0.0);
        assert_eq!(effective(Some(80.0), Some(101), 100), 80.0);
        assert_eq!(effective(None, None, 100), 0.0);
        assert_eq!(effective(Some(120.0), None, 0), 100.0);
    }

    #[test]
    fn trigger_prefers_weekly() {
        assert_eq!(
            threshold_trigger(
                Usage {
                    five_hour: 99.0,
                    weekly: 99.0
                },
                T
            ),
            Some(Reason::Weekly)
        );
        assert_eq!(
            threshold_trigger(
                Usage {
                    five_hour: 98.0,
                    weekly: 10.0
                },
                T
            ),
            Some(Reason::FiveHour)
        );
        assert_eq!(
            threshold_trigger(
                Usage {
                    five_hour: 97.9,
                    weekly: 97.9
                },
                T
            ),
            None
        );
    }

    #[test]
    fn picks_lowest_weekly_then_five_hour_then_priority() {
        let a = acct(1, "a", AccountState::Parked, 0);
        let b = acct(2, "b", AccountState::Parked, 0);
        let c = acct(3, "c", AccountState::Parked, -1);
        let d = acct(4, "d", AccountState::Paused, 0);
        let e = acct(5, "e", AccountState::NeedsLogin, 0);
        let cands = vec![
            Candidate {
                account: &a,
                usage: Usage {
                    five_hour: 10.0,
                    weekly: 40.0,
                },
            },
            Candidate {
                account: &b,
                usage: Usage {
                    five_hour: 5.0,
                    weekly: 40.0,
                },
            },
            Candidate {
                account: &c,
                usage: Usage {
                    five_hour: 5.0,
                    weekly: 40.0,
                },
            },
            Candidate {
                account: &d,
                usage: Usage {
                    five_hour: 0.0,
                    weekly: 0.0,
                },
            },
            Candidate {
                account: &e,
                usage: Usage {
                    five_hour: 0.0,
                    weekly: 0.0,
                },
            },
        ];
        assert_eq!(pick(&cands, T, None).unwrap().id, 3);
        assert_eq!(pick(&cands, T, Some(3)).unwrap().id, 2);
    }

    #[test]
    fn exhausted_accounts_are_not_eligible() {
        let a = acct(1, "a", AccountState::Parked, 0);
        let b = acct(2, "b", AccountState::Parked, 0);
        let cands = vec![
            Candidate {
                account: &a,
                usage: Usage {
                    five_hour: 99.0,
                    weekly: 10.0,
                },
            },
            Candidate {
                account: &b,
                usage: Usage {
                    five_hour: 10.0,
                    weekly: 98.0,
                },
            },
        ];
        assert!(pick(&cands, T, None).is_none());
    }
}
