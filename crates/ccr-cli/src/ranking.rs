//! The order accounts are used in, and why: `order`, `move`, `weekly-limit` and `next`.

use anyhow::{Result, bail};
use ccr_core::store::{Store, WeeklyLimitSetting, kind};
use ccr_core::{Paths, scheduler, snapshot, swap};
use serde_json::json;

use crate::format;

/// `order` with no accounts prints the order; with some, puts them first in that order.
pub fn order(store: &Store, labels: &[String]) -> Result<()> {
    if !labels.is_empty() {
        let mut ids = Vec::with_capacity(labels.len());
        for label in labels {
            let id = store.find_account(label)?.id;
            if ids.contains(&id) {
                bail!("{label} is listed twice");
            }
            ids.push(id);
        }
        store.set_order(&ids)?;
        log_order(store)?;
    }
    print_order(store)
}

/// `move <account> --to <n>`.
pub fn move_to(store: &Store, label: &str, to: usize) -> Result<()> {
    if to == 0 {
        bail!("places start at 1");
    }
    let account = store.find_account(label)?;
    store.move_account(account.id, to)?;
    log_order(store)?;
    print_order(store)
}

/// `weekly-limit <account> [auto|yes|no]`.
pub fn weekly_limit(store: &Store, label: &str, value: Option<&str>) -> Result<()> {
    let account = store.find_account(label)?;
    if let Some(value) = value {
        let setting: WeeklyLimitSetting = value.parse()?;
        store.set_weekly_limit(account.id, setting)?;
        store.log_event(
            kind::STATE,
            Some(account.id),
            None,
            Some("weekly_limit"),
            Some(json!({ "label": account.label, "weekly_limit": setting.as_str() })),
        )?;
    }
    let account = store.find_account(label)?;
    let detected = swap::detect_weekly_limit(store, account.id, ccr_core::time::now_s())?;
    let effective = scheduler::weekly_limit(account.weekly_limit, detected);
    let says = |w: scheduler::WeeklyLimit| match w {
        scheduler::WeeklyLimit::None => "no weekly limit",
        scheduler::WeeklyLimit::Unknown => "not known yet",
        scheduler::WeeklyLimit::Has => "has a weekly limit",
    };
    match account.weekly_limit {
        WeeklyLimitSetting::Auto => println!(
            "{}: {} (detected from its usage)",
            account.label,
            says(effective)
        ),
        _ => println!(
            "{}: {} (set by you; its usage says: {})",
            account.label,
            says(effective),
            says(detected)
        ),
    }
    Ok(())
}

/// `next [--explain] [--json]`.
pub fn next(store: &Store, paths: &Paths, explain: bool, json: bool) -> Result<()> {
    // Count what the other profiles read since, as the next swap will.
    ccr_core::profile_usage::import(store, paths, ccr_core::time::now_ms())?;
    let snap = snapshot::snapshot(store, paths)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "next": snap.next, "ranking": snap.ranking }))?
        );
    } else {
        print!("{}", format::next(&snap, explain));
    }
    Ok(())
}

fn log_order(store: &Store) -> Result<()> {
    let labels: Vec<String> = store.accounts()?.into_iter().map(|a| a.label).collect();
    store.log_event(
        kind::STATE,
        None,
        None,
        Some("order"),
        Some(json!({ "order": labels })),
    )?;
    Ok(())
}

fn print_order(store: &Store) -> Result<()> {
    let accounts = store.accounts()?;
    if accounts.is_empty() {
        println!("No accounts yet. Add one with `cc-rotate-ctl login`.");
    }
    for (i, a) in accounts.iter().enumerate() {
        println!("{:>2}  {}", i + 1, a.label);
    }
    Ok(())
}
