//! Human-readable output for the terminal.

use std::fmt::Write;

use ccr_core::doctor::{Check, Status};
use ccr_core::scheduler::{WeeklyLimit, WindowKind, WindowUsage};
use ccr_core::snapshot::{AccountView, Snapshot};
use ccr_core::store::{AccountState, Event, Store};
use ccr_core::time;

const DIM: &str = "\x1b[2m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

pub fn statusline_prefix(label: &str, pool_exhausted: bool) -> String {
    if pool_exhausted {
        format!("{YELLOW}[{label}: pool empty]{RESET}")
    } else {
        format!("{DIM}[{label}]{RESET}")
    }
}

/// "3h 12m", "45m", "2d 4h" until `at_s`, or "now" when it has passed.
pub fn until(at_s: i64) -> String {
    time::span(at_s - time::now_s())
}

fn ago(ts_ms: i64) -> String {
    let secs = (time::now_ms() - ts_ms) / 1000;
    match secs {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

fn state_label(a: &AccountView) -> &'static str {
    match a.account.state {
        AccountState::Active => "active",
        AccountState::Parked if a.eligible => "ready",
        AccountState::Parked => "resting",
        AccountState::NeedsLogin => "needs login",
        AccountState::Paused => "paused",
    }
}

fn reset(at: Option<i64>) -> String {
    at.filter(|&t| t > time::now_s())
        .map(until)
        .unwrap_or_else(|| "-".into())
}

/// The weekly column: the reading, "none" for an account without a weekly limit, "-" without
/// a reading.
fn weekly_cells(a: &AccountView) -> (String, String) {
    let week = a
        .windows
        .iter()
        .find(|w| w.kind == WindowKind::Weekly && w.model.is_none());
    match (week, a.weekly.effective) {
        (Some(w), _) => (format!("{:.0}%", w.pct), reset(w.resets_at)),
        (None, WeeklyLimit::None) => ("none".into(), "-".into()),
        (None, _) => ("-".into(), "-".into()),
    }
}

/// Windows other than the 5-hour and weekly ones on all models, such as one model's weekly
/// limit: "Opus weekly 40% (resets in 2d 3h)".
fn other_windows(a: &AccountView) -> Vec<&WindowUsage> {
    a.windows
        .iter()
        .filter(|w| w.model.is_some() || w.kind == WindowKind::Other)
        .collect()
}

pub fn status(snap: &Snapshot) -> String {
    let mut out = String::new();
    if snap.accounts.is_empty() {
        let _ = writeln!(out, "No accounts yet. Add one with `cc-rotate-ctl login`.");
        return out;
    }
    let width = snap
        .accounts
        .iter()
        .map(|a| a.account.label.len())
        .max()
        .unwrap_or(5)
        .max(5);
    let plan_width = snap
        .accounts
        .iter()
        .filter_map(|a| a.plan.as_ref().map(String::len))
        .max()
        .unwrap_or(4)
        .max(4);
    let _ = writeln!(
        out,
        "{:>2}  {:<width$}  {:<plan_width$}  {:<11}  {:>5}  {:<9}  {:>5}  {:<9}  email",
        "#", "label", "plan", "state", "5h", "resets", "week", "resets"
    );
    for (i, a) in snap.accounts.iter().enumerate() {
        let five = a
            .windows
            .iter()
            .find(|w| w.kind == WindowKind::Session && w.model.is_none());
        let (week, week_reset) = weekly_cells(a);
        let _ = writeln!(
            out,
            "{:>2}  {:<width$}  {:<plan_width$}  {:<11}  {:>5}  {:<9}  {:>5}  {:<9}  {}",
            i + 1,
            a.account.label,
            a.plan.as_deref().unwrap_or("-"),
            state_label(a),
            five.map_or("-".into(), |w| format!("{:.0}%", w.pct)),
            reset(five.and_then(|w| w.resets_at)),
            week,
            week_reset,
            a.account.email.as_deref().unwrap_or("-"),
        );
        for w in other_windows(a) {
            let _ = writeln!(
                out,
                "{:>2}  {:<width$}  {DIM}{} {:.0}%{}{}{RESET}",
                "",
                "",
                w.label,
                w.pct,
                w.resets_at
                    .filter(|&t| t > time::now_s())
                    .map(|t| format!(", resets in {}", until(t)))
                    .unwrap_or_default(),
                if w.applies {
                    ""
                } else {
                    ", not for the model in use"
                },
            );
        }
    }
    let p = &snap.pool;
    let _ = writeln!(out);
    if p.weekly_limited > 0 {
        let _ = write!(
            out,
            "Pool: {:.1} of {} accounts' weekly budget left",
            p.weekly_accounts_left, p.weekly_limited
        );
        if p.no_weekly_limit > 0 {
            let _ = write!(out, ", plus {} without a weekly limit", p.no_weekly_limit);
        }
    } else {
        let _ = write!(out, "Pool: {} accounts, none with a weekly limit", p.usable);
    }
    let _ = writeln!(out, ", {} ready, {} swaps in 24h.", p.eligible, p.swaps_24h);
    if let Some(next) = &snap.next {
        let _ = writeln!(out, "Next: {} - {}", next.label, next.reason);
    }
    if p.exhausted {
        let next = p
            .next_available_at
            .map(until)
            .unwrap_or_else(|| "unknown".into());
        let _ = writeln!(
            out,
            "{YELLOW}No other account is available. Next one frees up in {next}.{RESET}"
        );
    }
    if !p.needs_login.is_empty() {
        let _ = writeln!(
            out,
            "{YELLOW}Needs a new login: {} (cc-rotate-ctl login --relogin <label>){RESET}",
            p.needs_login.join(", ")
        );
    }
    out
}

/// `next`: the account the next swap would pick, and with `explain`, every account in the
/// order the scheduler would use them.
pub fn next(snap: &Snapshot, explain: bool) -> String {
    let mut out = String::new();
    match &snap.next {
        Some(n) => {
            let _ = writeln!(out, "{} - {}", n.label, n.reason);
        }
        None if snap.accounts.is_empty() => {
            let _ = writeln!(out, "No accounts yet. Add one with `cc-rotate-ctl login`.");
            return out;
        }
        None => {
            let _ = writeln!(
                out,
                "None: every other account is paused, needs a login, or is over a threshold"
            );
        }
    }
    if !explain {
        return out;
    }
    let width = snap
        .ranking
        .iter()
        .map(|r| r.label.len())
        .max()
        .unwrap_or(5)
        .max(5);
    let _ = writeln!(out);
    for r in &snap.ranking {
        let view = snap.accounts.iter().find(|a| a.account.id == r.account_id);
        let state = view.map(state_label).unwrap_or("-");
        let place = r.rank.map_or("-".to_string(), |n| n.to_string());
        let marker = if snap
            .next
            .as_ref()
            .is_some_and(|n| n.account_id == r.account_id)
        {
            "  <- next"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "{place:>2}  {:<width$}  {state:<11}  {}{marker}",
            r.label, r.reason
        );
    }
    let s = &snap.settings;
    let _ = writeln!(
        out,
        "\n{DIM}Order: {}{RESET}",
        if s.prefer_no_weekly_limit {
            "accounts without a weekly limit first, then your order, then the most room."
        } else {
            "your order, then the most room."
        }
    );
    out
}

pub fn events(store: &Store, events: &[Event]) -> String {
    let label = |id: Option<i64>| {
        id.and_then(|i| store.account(i).ok().flatten())
            .map(|a| a.label)
            .unwrap_or_else(|| "-".into())
    };
    let mut out = String::new();
    for e in events.iter().rev() {
        let detail = e
            .detail
            .as_ref()
            .and_then(|d| d.get("why").or_else(|| d.get("message")))
            .and_then(|v| v.as_str())
            .map(|s| format!("  {DIM}{s}{RESET}"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "{:>8}  {:<14} {:<19} {} -> {}{}",
            ago(e.ts),
            e.kind,
            e.reason.as_deref().unwrap_or(""),
            label(e.account_from),
            label(e.account_to),
            detail
        );
    }
    out
}

pub fn doctor(checks: &[Check]) -> String {
    let width = checks
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for c in checks {
        let mark = match c.status {
            Status::Ok => "\x1b[32mok  \x1b[0m",
            Status::Warn => "\x1b[33mwarn\x1b[0m",
            Status::Fail => "\x1b[31mfail\x1b[0m",
        };
        let _ = writeln!(out, "{mark}  {:<width$}  {}", c.name, c.detail);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every detail starts in the same column, however long the longest check's name is.
    #[test]
    fn doctor_details_line_up() {
        let check = |name: &str| Check {
            name: name.into(),
            status: Status::Ok,
            detail: "detail".into(),
        };
        let out = doctor(&[check("Profile"), check("Claude Code login storage")]);
        let columns: Vec<usize> = out.lines().map(|l| l.find("detail").unwrap()).collect();
        assert_eq!(columns[0], columns[1], "{out}");
    }
}
