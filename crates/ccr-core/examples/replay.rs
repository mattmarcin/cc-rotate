//! Replay recorded use through the scheduler, to compare rotation policies on real history.
//!
//!   cargo run -p ccr-core --example replay -- [--store <store.db>] [--days N]
//!
//! The demand comes from the cc-rotate profile's status line readings: every rise of an
//! account's 5-hour window, in hours of that account's usual pace (`budget::Rates`), spread
//! over the minutes since the reading before it. The pool is then played forward minute by
//! minute with the real `scheduler::decide` and `scheduler::pick`, once per policy: the account
//! in use spends the demand, 5-hour windows start with their first use (floored to a 10-minute
//! mark) and reset 5 hours later, weekly windows reset a week after their reset, and accounts
//! whose weekly budget would reset unused move ahead as they do live. Ticks with demand get
//! the threshold triggers; the others only what the app's background sync applies.
//!
//! Other profiles' readings, usage checks and model-scoped windows other than the starting
//! readings are left out, though a rise after the rotation profile returns to an account
//! includes other profiles' use in between. Weekly limits are detected as of the end of the
//! period, and no account starts out moved ahead for its weekly budget. So the numbers compare policies; they do not predict the pool.
//!
//! The store (by default the real one) is copied with `VACUUM INTO` from a read-only
//! connection, and only the copy is opened as a `Store`, so the real one is never migrated or
//! written (SQLite may create its `-wal` and `-shm` files).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use ccr_core::Store;
use ccr_core::budget::{self, Rates};
use ccr_core::paths::Paths;
use ccr_core::scheduler::{self, Candidate, Policy, Reason, Timing, WindowKind};
use ccr_core::store::{AccountState, Sample, SampleWindow, kind};
use ccr_core::{swap, time};

const STEP_S: i64 = 60;
const FIVE_HOUR_S: i64 = 5 * 3600;
const WEEK_S: i64 = 7 * 86_400;
/// A rise is spread over at most this long before it, like the rate learning's longest gap.
const MAX_SPREAD_S: i64 = 20 * 60;

fn main() -> Result<()> {
    let mut store_path: Option<PathBuf> = None;
    let mut days: i64 = 7;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--store" => store_path = Some(args.next().context("--store needs a path")?.into()),
            "--days" => days = args.next().context("--days needs a number")?.parse()?,
            _ => bail!("usage: replay [--store <store.db>] [--days N]"),
        }
    }
    let source = match store_path {
        Some(p) => p,
        None => Paths::default_data_dir()?.join("store.db"),
    };
    let dir = std::env::temp_dir().join(format!("cc-rotate-replay-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let copy = dir.join("store.db");
    let result = snapshot(&source, &copy).and_then(|()| replay(&Store::open(&copy)?, days));
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// A consistent copy of the store at `source`, read through a read-only connection.
fn snapshot(source: &std::path::Path, copy: &std::path::Path) -> Result<()> {
    let conn = rusqlite::Connection::open_with_flags(
        source,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {} read-only", source.display()))?;
    conn.execute("VACUUM INTO ?1", [copy.to_string_lossy().as_ref()])?;
    Ok(())
}

/// One account as the replay plays it.
#[derive(Clone)]
struct Sim {
    id: i64,
    windows: Vec<SampleWindow>,
}

#[derive(Default)]
struct Outcome {
    /// Hours of use at the usual pace that the pool served, and that it could not.
    served: f64,
    unserved: f64,
    swaps: BTreeMap<&'static str, usize>,
    /// 5-hour windows started, by account label.
    windows_started: BTreeMap<String, usize>,
    /// 5-hour points left unused in windows that reset after being used.
    points_lost: f64,
}

fn replay(store: &Store, days: i64) -> Result<()> {
    let end_ms = store
        .samples_since(0)?
        .iter()
        .filter(|s| s.profile == "rotate")
        .map(|s| s.ts)
        .max()
        .context("no readings from the cc-rotate profile")?;
    let start_ms = end_ms - days * 86_400_000;
    let accounts = store.accounts()?;
    let mut by_account = Vec::new();
    for a in &accounts {
        by_account.push((
            a.id,
            store.account_samples_since(a.id, end_ms - budget::LEARN_DAYS * 86_400_000)?,
        ));
    }
    let rates = budget::learn(&by_account, end_ms);

    let demand = demand(&by_account, &rates, start_ms, end_ms);
    let first_rotate = by_account
        .iter()
        .flat_map(|(_, s)| s.iter())
        .filter(|s| s.profile == "rotate" && s.ts >= start_ms)
        .min_by_key(|s| s.ts)
        .context("no readings in the period")?;
    let start_s = start_ms.max(first_rotate.ts) / 1000;
    let end_s = end_ms / 1000;
    // `play` starts at the minute `start_s` falls in.
    let total: f64 = demand
        .range(start_s - start_s % STEP_S..end_s)
        .map(|(_, d)| d)
        .sum();
    println!(
        "Replaying {} to {} ({:.1} h of readings): {:.1} hours of use at the usual pace.",
        time::local(start_s, "%a %b %-d %H:%M"),
        time::local(end_s, "%a %b %-d %H:%M"),
        (end_s - start_s) as f64 / 3600.0,
        total
    );
    for a in &accounts {
        let r = rates.account(a.id);
        println!(
            "  {:<8} {:>5.1} five-hour points an hour, {:.3} weekly points per five-hour point",
            a.label, r.five_per_hour, r.weekly_per_5h
        );
    }

    // Each account's latest reading at the start, from any source.
    let start: Vec<Sim> = by_account
        .iter()
        .map(|(id, samples)| Sim {
            id: *id,
            windows: samples
                .iter()
                .filter(|s| s.ts <= start_s * 1000)
                .max_by_key(|s| s.ts)
                .map(|s| s.windows.clone())
                .unwrap_or_default(),
        })
        .collect();

    let recorded = store
        .events(100_000)?
        .into_iter()
        .filter(|e| e.kind == kind::SWAP && e.ts >= start_s * 1000 && e.ts <= end_ms)
        .fold(BTreeMap::<String, usize>::new(), |mut m, e| {
            *m.entry(e.reason.unwrap_or_default()).or_default() += 1;
            m
        });
    println!("\nRecorded swaps in the period: {recorded:?}");

    let settings = store.settings()?;
    for strict in [false, true] {
        let policy = Policy {
            strict_order: strict,
            ..Policy::from(&settings)
        };
        let out = play(
            store,
            policy,
            &settings,
            &rates,
            &demand,
            start.clone(),
            first_rotate.account_id,
            start_s,
            end_s,
        )?;
        println!(
            "\n{}: served {:.2} h, not served {:.2} h ({:.1}%)",
            if strict {
                "strict_order"
            } else {
                "running windows first"
            },
            out.served,
            out.unserved,
            100.0 * out.unserved / (out.served + out.unserved).max(f64::EPSILON)
        );
        println!(
            "  swaps: {} {:?}",
            out.swaps.values().sum::<usize>(),
            out.swaps
        );
        println!("  5-hour windows started: {:?}", out.windows_started);
        println!(
            "  5-hour points left unused in windows that were used: {:.0}",
            out.points_lost
        );
    }
    Ok(())
}

/// Hours of use at the usual pace in each minute (epoch seconds of its start), from the rises of
/// the cc-rotate profile's readings.
fn demand(
    by_account: &[(i64, Vec<Sample>)],
    rates: &Rates,
    start_ms: i64,
    end_ms: i64,
) -> BTreeMap<i64, f64> {
    let mut out: BTreeMap<i64, f64> = BTreeMap::new();
    for (id, samples) in by_account {
        let usual = rates.account(*id).five_per_hour;
        // The highest reading of the current window: (reset, percent, when).
        let mut high: Option<(Option<i64>, f64, i64)> = None;
        for s in samples.iter().filter(|s| s.profile == "rotate") {
            let Some(w) = s.windows.iter().find(|w| w.name == scheduler::FIVE_HOUR) else {
                continue;
            };
            let ts = s.ts / 1000;
            match high {
                Some((reset, pct, _)) if same_reset(reset, w.resets_at) => {
                    if w.pct > pct && s.ts >= start_ms && s.ts <= end_ms {
                        let hours = (w.pct - pct) / usual;
                        let (_, _, last) = high.unwrap();
                        spread(&mut out, last.max(ts - MAX_SPREAD_S), ts, hours);
                    }
                    if w.pct >= pct {
                        high = Some((reset, w.pct, ts));
                    }
                }
                // A stale session's reading of an older window.
                Some((reset, _, _)) if older(w.resets_at, reset) => {}
                _ => {
                    // A new window: what it shows was used since it started.
                    if s.ts >= start_ms && s.ts <= end_ms && w.pct > 0.0 {
                        let started = w.resets_at.map_or(ts, |r| r - FIVE_HOUR_S);
                        spread(&mut out, started.max(ts - MAX_SPREAD_S), ts, w.pct / usual);
                    }
                    high = Some((w.resets_at, w.pct, ts));
                }
            }
        }
    }
    out
}

fn spread(out: &mut BTreeMap<i64, f64>, from_s: i64, to_s: i64, hours: f64) {
    let first = from_s.min(to_s) / STEP_S;
    let last = to_s / STEP_S;
    let n = (last - first + 1) as f64;
    for m in first..=last {
        *out.entry(m * STEP_S).or_default() += hours / n;
    }
}

fn same_reset(a: Option<i64>, b: Option<i64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() <= 60,
        (None, None) => true,
        _ => false,
    }
}

fn older(a: Option<i64>, b: Option<i64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a < b - 60,
        (None, Some(_)) => true,
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn play(
    store: &Store,
    policy: Policy,
    settings: &ccr_core::store::Settings,
    rates: &Rates,
    demand: &BTreeMap<i64, f64>,
    mut sims: Vec<Sim>,
    first_active: i64,
    start_s: i64,
    end_s: i64,
) -> Result<Outcome> {
    let accounts = store.accounts()?;
    let model = store.meta(swap::META_PROFILE_MODEL)?;
    let mut positions: Vec<i64> = accounts.iter().map(|a| a.position).collect();
    positions.dedup();
    let mut weekly = HashMap::new();
    for a in &accounts {
        let detected = swap::detect_weekly_limit(store, a.id, end_s)?;
        weekly.insert(
            a.id,
            (detected, scheduler::weekly_limit(a.weekly_limit, detected)),
        );
    }

    let mut out = Outcome::default();
    let mut active = first_active;
    // Accounts moved ahead for their weekly budget during the replay. The store's own marks
    // are not loaded: they are as of now, not as of the start.
    let mut promoted: HashMap<i64, i64> = HashMap::new();
    let mut timing = Timing {
        cooldown_ms: settings.swap_cooldown_s * 1000,
        dwell_ms: settings.switch_back_dwell_s * 1000,
        active_since_ms: start_s * 1000,
        ..Default::default()
    };
    // Points used in each account's running 5-hour window.
    let mut used_in_window: HashMap<i64, bool> = HashMap::new();

    let mut t = start_s - start_s % STEP_S;
    while t < end_s {
        // Windows reset.
        for s in &mut sims {
            for w in &mut s.windows {
                let Some(r) = w.resets_at.filter(|&r| r <= t) else {
                    continue;
                };
                match scheduler::classify(&w.name).kind {
                    WindowKind::Session => {
                        if w.name == scheduler::FIVE_HOUR
                            && used_in_window.remove(&s.id).unwrap_or(false)
                        {
                            out.points_lost += (settings.swap_threshold_5h - w.pct).max(0.0);
                        }
                        w.pct = 0.0;
                        w.resets_at = None;
                    }
                    _ => {
                        w.pct = 0.0;
                        w.resets_at = Some(r + WEEK_S);
                    }
                }
            }
            s.windows
                .retain(|w| !(w.name == scheduler::FIVE_HOUR && w.resets_at.is_none()));
        }

        let pool = candidates(
            &accounts,
            &sims,
            &positions,
            &weekly,
            active,
            policy,
            model.as_deref(),
            t,
        );
        let pool = with_expiring(pool, policy, rates, &mut promoted, active, t);

        let d = demand.get(&t).copied().unwrap_or(0.0);
        timing.now_ms = t * 1000;
        timing.arrived_over_limit = false;
        if let Some(reason) = scheduler::decide(&pool, policy, timing, false, d > 0.0) {
            timing.last_request_ms = timing.now_ms;
            let target = match reason {
                Reason::PreferredAvailable => scheduler::switch_back_target(&pool, policy, t),
                _ => scheduler::pick(&pool, policy, Some(active), t),
            };
            if let Some(target) = target {
                if let Some(e) = target.expiring {
                    promoted.insert(target.account.id, e.resets_at);
                }
                *out.swaps.entry(reason.as_str()).or_default() += 1;
                active = target.account.id;
                timing.last_swap_ms = timing.now_ms;
                timing.active_since_ms = timing.now_ms;
            }
        }

        if d > 0.0 {
            let current = pool.iter().find(|c| c.account.id == active).unwrap();
            let pool_now = candidates(
                &accounts,
                &sims,
                &positions,
                &weekly,
                active,
                policy,
                model.as_deref(),
                t,
            );
            let blocked = pool_now
                .iter()
                .find(|c| c.account.id == active)
                .is_some_and(|c| c.blocking().is_some());
            if blocked {
                out.unserved += d;
            } else {
                out.served += d;
                let r = rates.account(active);
                let five = d * r.five_per_hour;
                let sim = sims.iter_mut().find(|s| s.id == active).unwrap();
                if !sim.windows.iter().any(|w| w.name == scheduler::FIVE_HOUR) {
                    sim.windows.push(SampleWindow {
                        name: scheduler::FIVE_HOUR.into(),
                        pct: 0.0,
                        resets_at: None,
                    });
                }
                for w in &mut sim.windows {
                    match scheduler::classify(&w.name).kind {
                        WindowKind::Session if w.name == scheduler::FIVE_HOUR => {
                            if w.resets_at.is_none() {
                                w.resets_at = Some(t - t % 600 + FIVE_HOUR_S);
                                *out.windows_started
                                    .entry(current.account.label.clone())
                                    .or_default() += 1;
                            }
                            w.pct = (w.pct + five).min(100.0);
                            used_in_window.insert(active, true);
                        }
                        WindowKind::Weekly if w.name == scheduler::SEVEN_DAY => {
                            w.pct = (w.pct + five * r.weekly_per_5h).min(100.0);
                        }
                        _ => {}
                    }
                }
            }
        }
        t += STEP_S;
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn candidates(
    accounts: &[ccr_core::store::Account],
    sims: &[Sim],
    positions: &[i64],
    weekly: &HashMap<i64, (scheduler::WeeklyLimit, scheduler::WeeklyLimit)>,
    active: i64,
    policy: Policy,
    model: Option<&str>,
    now_s: i64,
) -> Vec<Candidate> {
    accounts
        .iter()
        .map(|a| {
            let mut account = a.clone();
            account.state = match a.state {
                AccountState::Paused | AccountState::NeedsLogin => a.state,
                _ if a.id == active => AccountState::Active,
                _ => AccountState::Parked,
            };
            let sim = sims.iter().find(|s| s.id == a.id).unwrap();
            let sample = Sample {
                ts: now_s * 1000,
                account_id: a.id,
                profile: "replay".into(),
                session_id: None,
                model: None,
                windows: sim.windows.clone(),
            };
            let (detected, limit) = weekly[&a.id];
            Candidate {
                windows: scheduler::window_usage(
                    Some(&sample),
                    now_s,
                    policy.thresholds.for_account(&account),
                    model,
                ),
                weekly_detected: detected,
                weekly_limit: limit,
                order: positions
                    .iter()
                    .position(|&p| p == a.position)
                    .unwrap_or_default(),
                held_back: false,
                expiring: None,
                account,
            }
        })
        .collect()
}

/// The pool with the accounts whose weekly budget would reset unused marked, as `load_pool`
/// does, keeping an account in use that is expiring moved ahead (`budget::note_in_use`).
fn with_expiring(
    mut pool: Vec<Candidate>,
    policy: Policy,
    rates: &Rates,
    promoted: &mut HashMap<i64, i64>,
    active: i64,
    now_s: i64,
) -> Vec<Candidate> {
    if !policy.use_expiring_weekly {
        return pool;
    }
    promoted.retain(|_, reset| *reset > now_s);
    let expiring = budget::expiring(&pool, rates, promoted, policy.thresholds.session, now_s);
    for c in &mut pool {
        c.expiring = expiring.get(&c.account.id).copied();
        if c.account.id == active {
            if let Some(e) = c.expiring {
                promoted.insert(active, e.resets_at);
            }
        }
    }
    pool
}
