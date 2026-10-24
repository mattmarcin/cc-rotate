//! Human-readable output for the terminal.

use std::fmt::Write;

use ccr_core::doctor::{Check, Status};
use ccr_core::snapshot::Snapshot;
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
    let secs = at_s - time::now_s();
    if secs <= 0 {
        return "now".into();
    }
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    match (d, h) {
        (0, 0) => format!("{}m", m.max(1)),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
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

fn state_label(state: AccountState) -> &'static str {
    match state {
        AccountState::Active => "active",
        AccountState::Parked => "ready",
        AccountState::NeedsLogin => "needs login",
        AccountState::Paused => "paused",
    }
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
    let _ = writeln!(
        out,
        "{:<width$}  {:<11}  {:>5}  {:<9}  {:>5}  {:<9}  email",
        "label", "state", "5h", "resets", "week", "resets"
    );
    for a in &snap.accounts {
        let reset = |at: Option<i64>| {
            at.filter(|&t| t > time::now_s())
                .map(until)
                .unwrap_or_else(|| "-".into())
        };
        let s = a.last_sample.as_ref();
        let _ = writeln!(
            out,
            "{:<width$}  {:<11}  {:>4.0}%  {:<9}  {:>4.0}%  {:<9}  {}",
            a.account.label,
            state_label(a.account.state),
            a.usage.five_hour,
            reset(s.and_then(|x| x.five_resets_at)),
            a.usage.weekly,
            reset(s.and_then(|x| x.week_resets_at)),
            a.account.email.as_deref().unwrap_or("-"),
        );
    }
    let p = &snap.pool;
    let _ = writeln!(
        out,
        "\nPool: {:.1} accounts of weekly budget left, {} ready, {} swaps in 24h.",
        p.weekly_accounts_left, p.eligible, p.swaps_24h
    );
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
            "{:>8}  {:<14} {:<12} {} -> {}{}",
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
    let mut out = String::new();
    for c in checks {
        let mark = match c.status {
            Status::Ok => "\x1b[32mok  \x1b[0m",
            Status::Warn => "\x1b[33mwarn\x1b[0m",
            Status::Fail => "\x1b[31mfail\x1b[0m",
        };
        let _ = writeln!(out, "{mark}  {:<20} {}", c.name, c.detail);
    }
    out
}
