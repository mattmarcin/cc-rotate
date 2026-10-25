//! Fill a store with fictional accounts, a week of usage and swap history, for trying the
//! dashboard without real logins and for screenshots.
//!
//!   CC_ROTATE_DATA_DIR=/tmp/cc-rotate-demo cargo run -p ccr-core --example demo_store
//!
//! Refuses to touch a store that already has accounts.

use anyhow::{Result, bail};
use ccr_core::Store;
use ccr_core::paths::Paths;
use ccr_core::store::{AccountIdentity, AccountState, Sample, SampleWindow, kind};
use ccr_core::time;
use serde_json::json;

/// (label, subscription type, rate limit tier, has a weekly limit)
const ACCOUNTS: &[(&str, &str, &str, bool)] = &[
    ("team-1", "team", "default_claude_max_5x", false),
    ("team-2", "team", "default_claude_max_5x", false),
    ("team-3", "team", "default_claude_team", false),
    ("team-4", "team", "default_claude_max_5x", false),
    ("personal", "max", "default_claude_max_20x", true),
    ("work", "max", "default_claude_max_5x", true),
    ("side", "pro", "default_claude_pro", true),
];

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
            week: 12.0 * i as f64 % 40.0,
            week_resets: (week_start + (8 + i as i64) * 24 * hour) / 1000,
        });
    }

    // A week of rotation the way the scheduler does it: the first account in the order whose
    // 5-hour window has room carries the load (seats without a weekly limit first), the pool
    // swaps on at the threshold, and switches back once an earlier account's window resets.
    let mut t = week_start;
    let mut current = 0usize;
    let mut since = t;
    while t < now {
        let t_s = t / 1000;
        for s in sims.iter_mut() {
            if s.five_resets != 0 && s.five_resets <= t_s {
                s.five = 0.0;
                s.five_resets = 0;
            }
        }
        let busy = (9..23).contains(&((t / hour) % 24));
        let usable = |s: &Sim| s.five < 98.0 && s.week < 98.0;
        let best = sims.iter().position(usable);
        let (next, reason) = if !usable(&sims[current]) {
            (best, "threshold_5h")
        } else {
            (
                best.filter(|&b| b < current && t - since >= hour / 6),
                "preferred_available",
            )
        };
        if let Some(next) = next.filter(|&n| n != current) {
            store.conn().execute(
                "INSERT INTO events (ts, kind, account_from, account_to, reason, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (
                    t,
                    kind::SWAP,
                    sims[current].id,
                    sims[next].id,
                    reason,
                    json!({}).to_string(),
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
            s.five += 7.0 + (t / 600_000 % 5) as f64;
            if s.weekly {
                s.week += 1.1;
            }
        }
        if s.five_resets != 0 {
            let mut windows = vec![SampleWindow {
                name: "five_hour".into(),
                pct: s.five.min(100.0),
                resets_at: Some(s.five_resets),
            }];
            if s.weekly {
                windows.push(SampleWindow {
                    name: "seven_day".into(),
                    pct: s.week.min(100.0),
                    resets_at: Some(s.week_resets),
                });
            }
            store.record_sample(&Sample {
                ts: t,
                account_id: s.id,
                profile: "rotate".into(),
                session_id: Some("demo".into()),
                model: Some("claude-opus-4-1".into()),
                windows,
            })?;
        }
        t += 20 * 60_000;
    }

    // Readings for the accounts that were never used, so every meter has something to show.
    for s in &sims {
        if store.last_sample(s.id)?.is_none() && s.weekly {
            store.record_sample(&Sample {
                ts: now - 2 * hour,
                account_id: s.id,
                profile: "rotate".into(),
                session_id: None,
                model: None,
                windows: vec![SampleWindow {
                    name: "seven_day".into(),
                    pct: s.week,
                    resets_at: Some(s.week_resets),
                }],
            })?;
        }
    }

    store.immediate(|st| {
        st.set_state(sims[current].id, AccountState::Active)?;
        st.set_state(sims[5].id, AccountState::Paused)?;
        st.set_state(sims[6].id, AccountState::NeedsLogin)?;
        st.set_meta("active_since", since)?;
        st.set_meta("last_swap_at", since)?;
        Ok(())
    })?;
    println!("Demo store written to {}", data.display());
    Ok(())
}
