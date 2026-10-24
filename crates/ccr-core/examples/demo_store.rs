//! Fill a store with fictional accounts, a week of usage and swap history, for trying the
//! dashboard without real logins and for screenshots.
//!
//!   CC_ROTATE_DATA_DIR=/tmp/cc-rotate-demo cargo run -p ccr-core --example demo_store
//!
//! Refuses to touch a store that already has accounts.

use anyhow::{Result, bail};
use ccr_core::Store;
use ccr_core::paths::Paths;
use ccr_core::store::{AccountIdentity, AccountState, Sample, kind};
use ccr_core::time;
use serde_json::json;

const NAMES: &[&str] = &[
    "main", "alt", "raid-1", "raid-2", "raid-3", "raid-4", "raid-5",
];

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
    let mut ids = Vec::new();
    for (i, name) in NAMES.iter().enumerate() {
        let ident = AccountIdentity {
            email: Some(format!("{name}@example.com")),
            account_uuid: Some(format!("demo-{i}")),
            subscription_type: Some("max".into()),
            ..Default::default()
        };
        let id = store.insert_account(name, &ident, AccountState::Parked)?;
        store.save_credential(
            id,
            &json!({
                "accessToken": format!("demo-access-{i}"),
                "refreshToken": format!("demo-refresh-{i}"),
                "expiresAt": now + 6 * hour,
                "subscriptionType": "max"
            }),
            "login",
        )?;
        ids.push(id);
    }

    // A week of rotation: each account carries the load for a stretch, its 5-hour window climbs
    // to the threshold, then the pool swaps to the next one.
    let week_start = now - 7 * 24 * hour;
    let mut weekly = [0.0f64; 7];
    let mut t = week_start;
    let mut current = 0usize;
    let mut five = 0.0f64;
    let mut window_reset = t + 5 * hour;
    while t < now {
        let busy = {
            let h = (t / hour) % 24;
            (9..23).contains(&h)
        };
        if busy {
            five += 6.5 + (t / 600_000 % 5) as f64;
            weekly[current] += 1.1;
        }
        if t >= window_reset {
            five = 0.0;
            window_reset = t + 5 * hour;
        }
        let id = ids[current];
        store.record_sample(&Sample {
            ts: t,
            account_id: id,
            profile: "rotate".into(),
            session_id: Some("demo".into()),
            five_pct: Some(five.min(100.0)),
            five_resets_at: Some(window_reset / 1000),
            week_pct: Some(weekly[current].min(100.0)),
            week_resets_at: Some((week_start + (8 + current as i64) * 24 * hour) / 1000),
        })?;
        if five >= 98.0 {
            let next = (current + 1) % ids.len();
            store.conn().execute(
                "INSERT INTO events (ts, kind, account_from, account_to, reason, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite_params(t, kind::SWAP, ids[current], ids[next]),
            )?;
            current = next;
            five = 0.0;
            window_reset = t + 5 * hour;
        }
        t += 20 * 60_000;
    }

    store.immediate(|s| {
        s.set_state(ids[current], AccountState::Active)?;
        s.set_state(ids[(current + 3) % ids.len()], AccountState::Paused)?;
        s.set_state(ids[(current + 5) % ids.len()], AccountState::NeedsLogin)?;
        Ok(())
    })?;
    println!("Demo store written to {}", data.display());
    Ok(())
}

fn rusqlite_params(ts: i64, kind: &str, from: i64, to: i64) -> impl rusqlite::Params {
    (
        ts,
        kind.to_string(),
        from,
        to,
        "threshold_5h".to_string(),
        json!({}).to_string(),
    )
}
