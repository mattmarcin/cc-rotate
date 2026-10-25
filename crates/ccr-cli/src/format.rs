//! Human-readable output for the terminal.

use std::fmt::Write;

use ccr_core::doctor::{Check, Status};
use ccr_core::history::{AccountHistory, HistoryKind};
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
        AccountState::Active => "in use",
        AccountState::Parked if a.eligible => "ready",
        AccountState::Parked => "resting",
        AccountState::NeedsLogin => "signed out",
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
            "Weekly usage left: {:.1} of {} accounts",
            p.weekly_accounts_left, p.weekly_limited
        );
        if p.no_weekly_limit > 0 {
            let _ = write!(out, ", plus {} without a weekly limit", p.no_weekly_limit);
        }
    } else {
        let _ = write!(out, "{} accounts, none with a weekly limit", p.usable);
    }
    let _ = writeln!(out, ", {} ready, {} swaps in 24h.", p.eligible, p.swaps_24h);
    if let Some(next) = &snap.next {
        let _ = writeln!(out, "Next: {} - {}", next.label, next.reason);
    }
    if let Some(f) = &snap.forecast {
        let now_s = snap.now_ms / 1000;
        match (f.runs_out_at, f.free_again_at) {
            (Some(out_at), free) => {
                let _ = write!(
                    out,
                    "{YELLOW}At this pace, every account is at a limit in {}",
                    time::span(out_at - now_s)
                );
                if let Some(free) = free {
                    let _ = write!(
                        out,
                        ", and one frees up {} later",
                        time::span(free - out_at)
                    );
                }
                let _ = writeln!(out, ".{RESET}");
            }
            (None, _) => {
                let _ = writeln!(out, "At this pace, there's room for the next 24 hours.");
            }
        }
    }
    if p.exhausted {
        let next = p
            .next_available_at
            .map(until)
            .unwrap_or_else(|| "unknown".into());
        let _ = writeln!(
            out,
            "{YELLOW}No other account is ready. The next one frees up in {next}.{RESET}"
        );
    }
    if !p.needs_login.is_empty() {
        let _ = writeln!(
            out,
            "{YELLOW}Signed out: {}. Sign in again with `cc-rotate-ctl login --relogin <label>`.{RESET}",
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
                "None: every other account is near a limit, paused or signed out"
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

/// "the last week", "the last 4 weeks".
fn last_weeks(weeks: usize) -> String {
    if weeks == 1 {
        "the last week".into()
    } else {
        format!("the last {weeks} weeks")
    }
}

/// " 2 weeks had few readings, so the real numbers may be higher."
fn thin_note(thin: usize) -> String {
    match thin {
        0 => String::new(),
        1 => " 1 week had few readings, so the real numbers may be higher.".into(),
        n => format!(" {n} weeks had few readings, so the real numbers may be higher."),
    }
}

/// One line on how much use an account got in the last `weeks` weeks.
pub fn report_summary(h: &AccountHistory, weeks: usize) -> String {
    let label = &h.label;
    let period = last_weeks(weeks);
    match h.kind {
        HistoryKind::Weekly => {
            let n = h.weekly.len();
            let Some(avg) = h.average_weekly_peak() else {
                return format!(
                    "{label}: its weekly limit hasn't reset in {period}, so there's nothing to report yet."
                );
            };
            let thin = h.weekly.iter().filter(|w| w.thin).count();
            let used = if n == 1 && weeks == 1 {
                format!("used {avg:.0}% of its weekly limit in {period}.")
            } else if n >= weeks {
                format!("used {avg:.0}% of its weekly limit on average over {period}.")
            } else if n == 1 {
                format!(
                    "used {avg:.0}% of its weekly limit in the only week with data in {period}."
                )
            } else {
                format!(
                    "used {avg:.0}% of its weekly limit on average over the {n} weeks with data in {period}."
                )
            };
            format!("{label}: {used}{}", thin_note(thin))
        }
        HistoryKind::FiveHour => {
            let known: Vec<_> = h
                .weeks
                .iter()
                .filter(|w| w.windows_used.is_some())
                .collect();
            if known.is_empty() {
                return format!("{label}: no usage recorded in {period}.");
            }
            let used: usize = known.iter().filter_map(|w| w.windows_used).sum();
            let at_limit: usize = known.iter().filter_map(|w| w.windows_at_limit).sum();
            let main = match (used, at_limit) {
                (0, _) => format!("didn't use a single 5-hour window in {period}."),
                (u, 0) => {
                    format!("none of its 5-hour windows reached the limit in {period} ({u} used).")
                }
                (u, r) if r == u => {
                    format!("all {u} of its 5-hour windows reached the limit in {period}.")
                }
                (u, r) => {
                    format!("{r} of its 5-hour windows reached the limit in {period} ({u} used).")
                }
            };
            let missing = h.weeks.len() - known.len();
            let missing = match missing {
                0 => String::new(),
                1 => " No data for 1 of those weeks.".into(),
                m => format!(" No data for {m} of those weeks."),
            };
            let thin = known.iter().filter(|w| w.thin).count();
            format!("{label}: {main}{missing}{}", thin_note(thin))
        }
    }
}

/// `report`: a table per account, then a one-line summary per account.
pub fn report(histories: &[AccountHistory], weeks: usize) -> String {
    let mut out = String::new();
    if histories.is_empty() {
        let _ = writeln!(out, "No accounts yet. Add one with `cc-rotate-ctl login`.");
        return out;
    }
    let mut any_thin = false;
    for h in histories {
        let kind = match h.kind {
            HistoryKind::Weekly => "weekly limit",
            HistoryKind::FiveHour if h.weekly_limit == WeeklyLimit::None => "no weekly limit",
            HistoryKind::FiveHour => "weekly limit not known yet",
        };
        let about = [h.plan.as_deref(), Some(kind)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "{}  {DIM}{about}{RESET}", h.label);
        match h.kind {
            HistoryKind::Weekly if h.weekly.is_empty() => {
                let _ = writeln!(
                    out,
                    "  {DIM}No weekly limit has reset in {}.{RESET}",
                    last_weeks(weeks)
                );
            }
            HistoryKind::Weekly => {
                let _ = writeln!(out, "  {:<12}  {:>5}  {:>8}", "reset", "peak", "readings");
                for w in &h.weekly {
                    any_thin |= w.thin;
                    let _ = writeln!(
                        out,
                        "  {:<12}  {:>5}  {:>8}{}",
                        time::local(w.resets_at, "%a %b %-d"),
                        format!("{:.0}%", w.peak_pct),
                        w.samples,
                        if w.thin {
                            format!("  {YELLOW}few readings{RESET}")
                        } else {
                            String::new()
                        },
                    );
                }
            }
            HistoryKind::FiveHour => {
                let _ = writeln!(
                    out,
                    "  {:<12}  {:>14}  {:>12}  {:>8}",
                    "week of", "5-hour windows", "at the limit", "readings"
                );
                for w in &h.weeks {
                    let count = |n: Option<usize>| n.map_or("-".to_string(), |n| n.to_string());
                    let note = match (w.windows_used, w.in_progress, w.thin) {
                        (None, _, _) => format!("  {DIM}no data{RESET}"),
                        (_, in_progress, true) => {
                            any_thin = true;
                            let so_far = if in_progress { "so far, " } else { "" };
                            format!("  {YELLOW}{so_far}few readings{RESET}")
                        }
                        (_, true, false) => format!("  {DIM}so far{RESET}"),
                        _ => String::new(),
                    };
                    let _ = writeln!(
                        out,
                        "  {:<12}  {:>14}  {:>12}  {:>8}{note}",
                        time::local(w.starts_at, "%a %b %-d"),
                        count(w.windows_used),
                        count(w.windows_at_limit),
                        w.samples,
                    );
                }
            }
        }
        let _ = writeln!(out);
    }
    for h in histories {
        let _ = writeln!(out, "{}", report_summary(h, weeks));
    }
    if any_thin {
        let _ = writeln!(
            out,
            "\n{DIM}Few readings: cc-rotate saw little of that week, so the real numbers may be higher.{RESET}"
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

    use ccr_core::history::{CalendarWeek, WeeklyWindow};

    fn history(label: &str, kind: HistoryKind) -> AccountHistory {
        AccountHistory {
            account_id: 1,
            label: label.into(),
            plan: Some("Max 5x".into()),
            weekly_limit: match kind {
                HistoryKind::Weekly => WeeklyLimit::Has,
                HistoryKind::FiveHour => WeeklyLimit::None,
            },
            kind,
            weekly: Vec::new(),
            weeks: Vec::new(),
            threshold_5h: 98.0,
        }
    }

    fn window(peak_pct: f64, thin: bool) -> WeeklyWindow {
        WeeklyWindow {
            resets_at: 1_792_368_000,
            peak_pct,
            samples: 50,
            last_sample_at: 1_792_368_000_000,
            thin,
        }
    }

    fn week(used: Option<usize>, at_limit: Option<usize>, thin: bool) -> CalendarWeek {
        CalendarWeek {
            starts_at: 1_792_368_000,
            ends_at: 1_792_972_800,
            in_progress: false,
            samples: if used.is_some() { 100 } else { 0 },
            windows_used: used,
            windows_at_limit: at_limit,
            thin,
        }
    }

    #[test]
    fn weekly_summaries() {
        let mut h = history("main", HistoryKind::Weekly);
        assert_eq!(
            report_summary(&h, 4),
            "main: its weekly limit hasn't reset in the last 4 weeks, so there's nothing to report yet."
        );
        h.weekly = vec![
            window(10.0, false),
            window(14.0, false),
            window(3.0, false),
            window(21.0, false),
        ];
        assert_eq!(
            report_summary(&h, 4),
            "main: used 12% of its weekly limit on average over the last 4 weeks."
        );
        h.weekly = vec![window(30.0, false), window(1.0, true)];
        assert_eq!(
            report_summary(&h, 4),
            "main: used 16% of its weekly limit on average over the 2 weeks with data in the last 4 weeks. 1 week had few readings, so the real numbers may be higher."
        );
        h.weekly = vec![window(1.0, false)];
        assert_eq!(
            report_summary(&h, 4),
            "main: used 1% of its weekly limit in the only week with data in the last 4 weeks."
        );
        assert_eq!(
            report_summary(&h, 1),
            "main: used 1% of its weekly limit in the last week."
        );
    }

    #[test]
    fn five_hour_summaries() {
        let mut h = history("account3", HistoryKind::FiveHour);
        h.weeks = vec![week(None, None, true); 4];
        assert_eq!(
            report_summary(&h, 4),
            "account3: no usage recorded in the last 4 weeks."
        );
        h.weeks = vec![
            week(Some(12), Some(9), false),
            week(Some(19), Some(0), false),
        ];
        assert_eq!(
            report_summary(&h, 2),
            "account3: 9 of its 5-hour windows reached the limit in the last 2 weeks (31 used)."
        );
        h.weeks = vec![week(Some(9), Some(9), false)];
        assert_eq!(
            report_summary(&h, 1),
            "account3: all 9 of its 5-hour windows reached the limit in the last week."
        );
        h.weeks = vec![
            week(Some(4), Some(0), false),
            week(Some(0), Some(0), true),
            week(None, None, true),
        ];
        assert_eq!(
            report_summary(&h, 3),
            "account3: none of its 5-hour windows reached the limit in the last 3 weeks (4 used). No data for 1 of those weeks. 1 week had few readings, so the real numbers may be higher."
        );
        h.weeks = vec![week(Some(0), Some(0), false)];
        assert_eq!(
            report_summary(&h, 1),
            "account3: didn't use a single 5-hour window in the last week."
        );
    }

    #[test]
    fn report_lists_every_account_then_the_summaries() {
        let mut main = history("main", HistoryKind::Weekly);
        main.weekly = vec![window(34.0, false), window(12.0, true)];
        let mut other = history("account3", HistoryKind::FiveHour);
        other.weeks = vec![week(Some(3), Some(1), false), week(None, None, true)];
        let out = report(&[main, other], 2);
        let main_at = out.find("main  ").unwrap();
        let other_at = out.find("account3  ").unwrap();
        let summary_at = out.find("main: used 23%").unwrap();
        assert!(main_at < other_at && other_at < summary_at, "{out}");
        assert!(out.contains("34%") && out.contains("few readings") && out.contains("no data"));
        assert!(!out.contains('\u{2014}'), "no em dashes");
        assert_eq!(
            report(&[], 4),
            "No accounts yet. Add one with `cc-rotate-ctl login`.\n"
        );
    }
}
