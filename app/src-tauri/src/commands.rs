//! Tauri commands: thin wrappers over ccr-core. Each opens its own store connection, and
//! anything that can wait on locks runs on the blocking pool.

use ccr_core::doctor::{self, Check};
use ccr_core::history::{self, AccountHistory};
use ccr_core::profiles::{self, FoundProfile};
use ccr_core::scheduler::Reason;
use ccr_core::snapshot::{self, Snapshot};
use ccr_core::store::{AccountState, Event, HourlyCost, Sample, WeeklyLimitSetting, kind};
use ccr_core::swap::{self, SwapOutcome, SwapRequest};
use ccr_core::usage_check::{self, CheckOutcome, Trigger};
use ccr_core::{Paths, Store};
use serde::Serialize;
use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_opener::OpenerExt;

pub type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    format!("{e:#}")
}

pub fn with_store<T>(f: impl FnOnce(&Store, &Paths) -> anyhow::Result<T>) -> CmdResult<T> {
    let (store, paths) = ccr_core::open(None).map_err(err)?;
    f(&store, &paths).map_err(|e| format!("{e:#}"))
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce(&Store, &Paths) -> anyhow::Result<T> + Send + 'static,
) -> CmdResult<T> {
    tauri::async_runtime::spawn_blocking(move || with_store(f))
        .await
        .map_err(err)?
}

#[tauri::command]
pub async fn snapshot() -> CmdResult<Snapshot> {
    blocking(snapshot::snapshot).await
}

#[tauri::command]
pub async fn series(since_ms: i64) -> CmdResult<Vec<Sample>> {
    blocking(move |s, _| snapshot::series(s, since_ms)).await
}

/// The swaps since `since_ms`, for the usage chart's markers.
#[tauri::command]
pub async fn swaps(since_ms: i64) -> CmdResult<Vec<Event>> {
    blocking(move |s, _| snapshot::swaps(s, since_ms)).await
}

/// What each account's use would have cost at API prices, hour by hour, for the cost chart.
#[tauri::command]
pub async fn costs(since_ms: i64) -> CmdResult<Vec<HourlyCost>> {
    blocking(move |s, _| snapshot::costs(s, since_ms)).await
}

#[tauri::command]
pub async fn events(limit: usize) -> CmdResult<Vec<Event>> {
    blocking(move |s, _| snapshot::recent_events(s, limit)).await
}

/// Weeks of history "Recent weeks" in the account dialog shows, besides the current one.
const RECENT_WEEKS: usize = 4;

/// An account's usage over the last weeks, for "Recent weeks" in its dialog. Read-only.
#[tauri::command]
pub async fn usage_history(id: i64) -> CmdResult<AccountHistory> {
    blocking(move |s, _| {
        let now = ccr_core::time::now_ms();
        let weeks = history::local_weeks(now / 1000, RECENT_WEEKS + 1);
        history::account_report(s, id, now, &weeks)
    })
    .await
}

#[tauri::command]
pub async fn swap_now(to: Option<i64>) -> CmdResult<SwapOutcome> {
    blocking(move |s, p| {
        swap::run(
            s,
            p,
            &SwapRequest {
                from: None,
                to,
                reason: Reason::Manual,
                wait: true,
            },
        )
    })
    .await
}

/// Read an account's usage now with Claude Code's `/usage`.
#[tauri::command]
pub async fn check_usage(id: i64) -> CmdResult<CheckOutcome> {
    blocking(move |s, p| usage_check::check(s, p, id, Trigger::Manual)).await
}

#[tauri::command]
pub async fn set_paused(id: i64, paused: bool) -> CmdResult<()> {
    blocking(move |s, _| {
        let account = s
            .account(id)?
            .ok_or_else(|| anyhow::anyhow!("That account no longer exists."))?;
        let next = match (account.state, paused) {
            (AccountState::Active, true) => {
                anyhow::bail!("Swap to another account before pausing {}.", account.label)
            }
            (AccountState::Parked, true) => AccountState::Paused,
            (AccountState::Paused, false) => AccountState::Parked,
            (state, _) => state,
        };
        if next != account.state {
            s.set_state(id, next)?;
            s.log_event(kind::STATE, Some(id), None, Some(next.as_str()), None)?;
        }
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn remove_account(id: i64) -> CmdResult<()> {
    blocking(move |s, p| {
        let account = s
            .account(id)?
            .ok_or_else(|| anyhow::anyhow!("That account no longer exists."))?;
        if account.state == AccountState::Active {
            anyhow::bail!("Swap to another account before removing {}.", account.label);
        }
        usage_check::remove_account(s, p, id)?;
        s.log_event(
            kind::STATE,
            Some(id),
            None,
            Some("removed"),
            Some(serde_json::json!({ "label": account.label })),
        )?;
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn rename_account(id: i64, label: String) -> CmdResult<()> {
    blocking(move |s, _| {
        if s.account_by_label(&label)?.is_some_and(|a| a.id != id) {
            anyhow::bail!("Another account is already named {label}.");
        }
        s.rename_account(id, &label)
    })
    .await
}

/// The order accounts are used in: `ids` first, in that order, then any others.
#[tauri::command]
pub async fn set_order(ids: Vec<i64>) -> CmdResult<()> {
    blocking(move |s, _| {
        s.set_order(&ids)?;
        let labels: Vec<String> = s.accounts()?.into_iter().map(|a| a.label).collect();
        s.log_event(
            kind::STATE,
            None,
            None,
            Some("order"),
            Some(serde_json::json!({ "order": labels })),
        )?;
        Ok(())
    })
    .await
}

/// Whether an account has a weekly limit: `auto` (detect it), `yes` or `no`.
#[tauri::command]
pub async fn set_weekly_limit(id: i64, value: String) -> CmdResult<()> {
    blocking(move |s, _| {
        let setting: WeeklyLimitSetting = value.parse()?;
        let account = s
            .account(id)?
            .ok_or_else(|| anyhow::anyhow!("That account no longer exists."))?;
        s.set_weekly_limit(id, setting)?;
        s.log_event(
            kind::STATE,
            Some(id),
            None,
            Some("weekly_limit"),
            Some(serde_json::json!({ "label": account.label, "weekly_limit": setting.as_str() })),
        )?;
        Ok(())
    })
    .await
}

/// Rotation stops using the account at this weekly usage (percent); `None` uses the setting.
#[tauri::command]
pub async fn set_weekly_cap(id: i64, cap: Option<f64>) -> CmdResult<()> {
    blocking(move |s, _| {
        let account = s
            .account(id)?
            .ok_or_else(|| anyhow::anyhow!("That account no longer exists."))?;
        s.set_weekly_cap(id, cap)?;
        s.log_event(
            kind::STATE,
            Some(id),
            None,
            Some("weekly_cap"),
            Some(serde_json::json!({ "label": account.label, "weekly_cap": cap })),
        )?;
        Ok(())
    })
    .await
}

/// Whether the account moves ahead when its weekly budget would reset unused.
#[tauri::command]
pub async fn set_use_expiring(id: i64, on: bool) -> CmdResult<()> {
    blocking(move |s, _| {
        let account = s
            .account(id)?
            .ok_or_else(|| anyhow::anyhow!("That account no longer exists."))?;
        s.set_use_expiring(id, on)?;
        s.log_event(
            kind::STATE,
            Some(id),
            None,
            Some("use_expiring"),
            Some(serde_json::json!({ "label": account.label, "use_expiring": on })),
        )?;
        Ok(())
    })
    .await
}

/// Whether cc-rotate keeps the account's 5-hour window running (spec section 22).
#[tauri::command]
pub async fn set_keep_running(id: i64, on: bool) -> CmdResult<()> {
    blocking(move |s, _| {
        let account = s
            .account(id)?
            .ok_or_else(|| anyhow::anyhow!("That account no longer exists."))?;
        s.set_keep_running(id, on)?;
        s.log_event(
            kind::STATE,
            Some(id),
            None,
            Some("keep_running"),
            Some(serde_json::json!({ "label": account.label, "keep_running": on })),
        )?;
        Ok(())
    })
    .await
}

/// Other Claude Code profiles on this computer, with the account each belongs to.
#[tauri::command]
pub async fn other_profiles() -> CmdResult<Vec<FoundProfile>> {
    blocking(profiles::discover).await
}

#[tauri::command]
pub async fn doctor() -> CmdResult<Vec<Check>> {
    blocking(|s, p| Ok(doctor::run(s, p))).await
}

#[tauri::command]
pub async fn set_setting(key: String, value: Option<String>) -> CmdResult<()> {
    blocking(move |s, _| s.set_setting(&key, value.as_deref().filter(|v| !v.is_empty()))).await
}

#[derive(Serialize)]
pub struct AppInfo {
    version: &'static str,
    claude_version: Option<String>,
    tested_versions: &'static [&'static str],
    profile_dir: String,
    data_dir: String,
}

#[tauri::command]
pub async fn app_info() -> CmdResult<AppInfo> {
    blocking(|s, p| {
        Ok(AppInfo {
            version: env!("CARGO_PKG_VERSION"),
            claude_version: doctor::claude_version(s),
            tested_versions: doctor::TESTED_CLAUDE_VERSIONS,
            profile_dir: p.profile_dir.display().to_string(),
            data_dir: p.data_dir.display().to_string(),
        })
    })
    .await
}

#[tauri::command]
pub fn autostart_enabled(app: AppHandle) -> CmdResult<bool> {
    app.autolaunch().is_enabled().map_err(err)
}

#[tauri::command]
pub fn set_autostart(app: AppHandle, enabled: bool) -> CmdResult<()> {
    let launcher = app.autolaunch();
    if enabled {
        launcher.enable()
    } else {
        launcher.disable()
    }
    .map_err(err)
}

#[tauri::command]
pub fn open_url(app: AppHandle, url: String) -> CmdResult<()> {
    if !url.starts_with("https://") {
        return Err("only https links can be opened".into());
    }
    app.opener().open_url(url, None::<&str>).map_err(err)
}
