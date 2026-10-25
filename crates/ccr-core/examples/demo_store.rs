//! Fill a store with fictional accounts, a week of usage and swap history, for trying the
//! dashboard without real logins and for screenshots.
//!
//!   CC_ROTATE_DATA_DIR=/tmp/cc-rotate-demo cargo run -p ccr-core --example demo_store
//!
//! Run the app against it with `CC_ROTATE_PROFILE` set to an empty directory too, so it never
//! touches your rotation profile. The store's `claude_path` points nowhere, so no usage check or
//! login ever starts Claude Code with the fictional logins.
//!
//! Refuses to touch a store that already has accounts.

use anyhow::{Result, bail};
use ccr_core::Store;
use ccr_core::paths::Paths;
use ccr_core::store::{AccountIdentity, AccountState, Sample, SampleWindow, kind};
use ccr_core::time;
use serde_json::json;

/// (label, subscription type, rate limit tier, has a weekly limit), in the user's order.
const ACCOUNTS: &[(&str, &str, &str, bool)] = &[
    ("main", "max", "default_claude_max_20x", true),
    ("account1", "team", "default_claude_max_5x", false),
    ("account2", "team", "default_claude_max_5x", false),
    ("account3", "team", "default_claude_max_5x", false),
    ("account4", "team", "default_claude_team", false),
    ("account5", "max", "default_claude_max_5x", true),
    ("account6", "pro", "default_claude_pro", true),
];

/// The account left paused.
const PAUSED: usize = 6;

struct Sim {
    id: i64,
    weekly: bool,
    five: f64,
    five_resets: i64,
    week: f64,
    week_resets: i64,
}

fn main() -> Result<()> {
    let data = Paths::default_data_dir()?;
    let store = Store::open(&data.join("store.db"))?;
    if !store.accounts()?.is_empty() {
        bail!(
            "{} already has accounts; point CC_ROTATE_DATA_DIR at an empty directory",
            data.display()
        );
    }
    let nowhere = data.join("no-claude-here");
    store.set_setting("claude_path", Some(&nowhere.display().to_string()))?;

    let now = time::now_ms();
    let hour = 3_600_000;
    let week_start = now - 7 * 24 * hour;
    let mut sims = Vec::new();
    for (i, (name, sub, tier, weekly)) in ACCOUNTS.iter().enumerate() {
        let ident = AccountIdentity {
            email: Some(format!("{name}@example.com")),
            account_uuid: Some(format!("demo-{i}")),
            subscription_type: Some(sub.to_string()),
            rate_limit_tier: Some(tier.to_string()),
            ..Default::default()
        };
        let id = store.insert_account(name, &ident, AccountState::Parked)?;
        store.save_credential(
            id,
            &json!({
                "accessToken": format!("demo-access-{i}"),
                "refreshToken": format!("demo-refresh-{i}"),
                "expiresAt": now + 6 * hour,
                "subscriptionType": sub,
                "rateLimitTier": tier,
            }),
            "login",
        )?;
        sims.push(Sim {
            id,
            weekly: *weekly,
            five: 0.0,
            five_resets: 0,
            week: 9.0 + 13.0 * i as f64 % 40.0,
            week_resets: (week_start + (8 + i as i64) * 24 * hour) / 1000,
        });
    }
    let ids: Vec<i64> = sims.iter().map(|s| s.id).collect();
    store.set_order(&ids)?;

    // The order the scheduler uses them in: accounts without a weekly limit first, then the
    // user's order. The paused account is never picked.
    let mut rank: Vec<usize> = (0..sims.len()).filter(|&i| i != PAUSED).collect();
    rank.sort_by_key(|&i| (sims[i].weekly, i));
    let place = |i: usize| rank.iter().position(|&r| r == i).unwrap_or(usize::MAX);

    // A week of rotation the way the scheduler does it: the first account in that order whose
    // 5-hour window has room carries the load, the pool swaps on at the threshold, after
    // checking the account it is about to use, and switches back once an earlier account's
    // window resets.
    let mut t = week_start;
    let mut current = rank[0];
    let mut since = t;
    while t < now {
        let t_s = t / 1000;
        for s in sims.iter_mut() {
            if s.five_resets != 0 && s.five_resets <= t_s {
                s.five = 0.0;
                s.five_resets = 0;
            }
        }
        // Working hours: the 15 hours before now, and the same hours on earlier days.
        let busy = (now - t) / hour % 24 < 15;
        let usable = |s: &Sim| s.five < 98.0 && s.week < 98.0;
        let best = rank.iter().copied().find(|&i| usable(&sims[i]));
        let (next, reason) = if !usable(&sims[current]) {
            (best, "threshold_5h")
        } else {
            (
                best.filter(|&b| place(b) < place(current) && t - since >= hour / 6),
                "preferred_available",
            )
        };
        if let Some(next) = next.filter(|&n| n != current) {
            let detail = if reason == "threshold_5h" {
                json!({ "checked": [{ "label": ACCOUNTS[next].0, "outcome": "read" }] })
            } else {
                json!({})
            };
            store.conn().execute(
                "INSERT INTO events (ts, kind, account_from, account_to, reason, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (
                    t,
                    kind::SWAP,
                    sims[current].id,
                    sims[next].id,
                    reason,
                    detail.to_string(),
                ),
            )?;
            current = next;
            since = t;
        }
        let s = &mut sims[current];
        if busy {
            if s.five_resets == 0 {
                s.five_resets = t_s + 5 * 3600;
            }
            s.five += 16.0 + (t / 600_000 % 5) as f64;
            if s.weekly {
                s.week += 1.1;
            }
        }
        if s.five_resets != 0 {
            store.record_sample(&Sample {
                ts: t,
                account_id: s.id,
                profile: "rotate".into(),
                session_id: Some("demo".into()),
                model: Some("claude-opus-4-1".into()),
                windows: windows(s),
            })?;
        }
        t += 20 * 60_000;
    }

    // A recent usage check of every account not in use, as the app makes in the background, so
    // every meter has a fresh reading and none is due for a check. Paused accounts are not
    // checked: the paused one keeps a reading from before it was paused.
    for (i, s) in sims.iter().enumerate() {
        if i == current {
            continue;
        }
        let paused = i == PAUSED;
        store.record_sample(&Sample {
            ts: if paused {
                now - 2 * 24 * hour
            } else {
                now - (12 + 7 * i as i64) * 60_000
            },
            account_id: s.id,
            profile: if paused { "rotate" } else { "check" }.into(),
            session_id: None,
            model: None,
            windows: checked_windows(s),
        })?;
    }

    store.immediate(|st| {
        st.set_state(sims[current].id, AccountState::Active)?;
        st.set_state(sims[PAUSED].id, AccountState::Paused)?;
        st.set_meta("active_since", since)?;
        st.set_meta("last_swap_at", since)?;
        Ok(())
    })?;
    println!("Demo store written to {}", data.display());
    Ok(())
}

/// What a usage check reports: the 5-hour window too while none is open, at 0% with no reset.
fn checked_windows(s: &Sim) -> Vec<SampleWindow> {
    let mut windows = windows(s);
    if s.five_resets == 0 {
        windows.insert(
            0,
            SampleWindow {
                name: "five_hour".into(),
                pct: 0.0,
                resets_at: None,
            },
        );
    }
    windows
}

/// The account's windows as Claude Code reports them: the 5-hour window while one is open, and
/// the weekly window for an account that has one.
fn windows(s: &Sim) -> Vec<SampleWindow> {
    let mut windows = Vec::new();
    if s.five_resets != 0 {
        windows.push(SampleWindow {
            name: "five_hour".into(),
            pct: s.five.min(100.0),
            resets_at: Some(s.five_resets),
        });
    }
    if s.weekly {
        windows.push(SampleWindow {
            name: "seven_day".into(),
            pct: s.week.min(100.0),
            resets_at: Some(s.week_resets),
        });
    }
    windows
}
