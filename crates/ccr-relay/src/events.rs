//! What the relay records in the store (spec 7, 8, 9 and 12): usage readings, threshold swaps,
//! swaps at a refused limit or login, resends and errors.

use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::Result;
use ccr_core::scheduler::{self, Reason};
use ccr_core::store::{Sample, kind};
use ccr_core::swap::{self, SwapOutcome, SwapRequest};
use ccr_core::{Paths, Store, relay, relay_proto, statusline, time, usage_check, waiting};
use serde_json::json;

use crate::{RelayEvents, Swapped};

/// The `profile` of the samples the relay records.
const RELAY_PROFILE: &str = "relay";

/// At most one `relay_error` event in this long.
const ERROR_EVERY_MS: i64 = 60_000;

/// Runs a swap. It takes file locks and may run Claude Code for checks, so it is only called on
/// a thread that may block.
pub trait Swaps: Send + Sync {
    fn run(&self, req: &SwapRequest) -> Result<SwapOutcome>;
}

/// `swap::run` on a store of its own, opened for each swap.
pub struct StoreSwaps {
    paths: Paths,
}

impl StoreSwaps {
    pub fn new(paths: Paths) -> StoreSwaps {
        StoreSwaps { paths }
    }
}

impl Swaps for StoreSwaps {
    fn run(&self, req: &SwapRequest) -> Result<SwapOutcome> {
        let store = Store::open(&self.paths.store_db())?;
        swap::run(&store, &self.paths, req)
    }
}

/// The relay's events, recorded in the store: a usage reading from every model response for the
/// account that served it, a threshold swap when the active account's reading asks for one, the
/// swap at a refused limit, resends and errors.
/// With relay mode off (a relay still serving until it retires, spec 4) the relay only forwards:
/// it records no reading, asks for no swap and holds no account back.
/// The status line does those then, and a swap writes the profile's login, which a session
/// started in relay mode does not send.
/// Log lines go on to `log` (a `FileLog` in the relay).
pub struct StoreEvents {
    /// For readings and events: quick writes, on the request threads.
    store: Mutex<Store>,
    swaps: Arc<dyn Swaps>,
    log: Arc<dyn RelayEvents>,
    /// Held while a refusal at a limit or a dead login is dealt with, so refusals of one account
    /// that arrive together ask for one swap: the others find the account no longer active.
    refusals: Mutex<()>,
    /// When the last `relay_error` event was recorded, in epoch ms.
    last_error_ms: Mutex<i64>,
}

impl StoreEvents {
    pub fn new(store: Store, swaps: Arc<dyn Swaps>, log: Arc<dyn RelayEvents>) -> StoreEvents {
        StoreEvents {
            store: Mutex::new(store),
            swaps,
            log,
            refusals: Mutex::new(()),
            last_error_ms: Mutex::new(0),
        }
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The active account, `None` when there is none or it cannot be read.
    fn active(&self) -> Option<i64> {
        match self.store().active_account() {
            Ok(active) => active.map(|a| a.id),
            Err(e) => {
                self.log(&format!("reading the active account failed: {e:#}"));
                None
            }
        }
    }

    /// Whether relay mode is on. An error reading it counts as off: the relay then only forwards.
    fn mode_on(&self, store: &Store) -> bool {
        match relay::home(store) {
            Ok(home) => home.is_some(),
            Err(e) => {
                self.log(&format!("reading the app account failed: {e:#}"));
                false
            }
        }
    }

    /// Records the reading in a response's headers (spec 7) and says which threshold swap to ask
    /// for, if any. Nothing with relay mode off.
    fn record(
        &self,
        account: i64,
        session: Option<&str>,
        model: Option<&str>,
        status: u16,
        headers: &[(String, String)],
    ) -> Result<Option<SwapRequest>> {
        let windows = relay_proto::readings(headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        if windows.is_empty() {
            return Ok(None);
        }
        let store = self.store();
        if !self.mode_on(&store) {
            return Ok(None);
        }
        let now = time::now_ms();
        let session = session.filter(|s| !s.is_empty());
        let sample = Sample {
            ts: now,
            account_id: account,
            profile: RELAY_PROFILE.into(),
            session_id: session.map(str::to_string),
            model: model.map(str::to_string),
            windows,
        };
        store.record_sample(&sample)?;
        // The status line records no readings in relay mode, so this is how a session stopped
        // at a limit is seen working again (main 21).
        if let Some(session) = session {
            waiting::note_reading(&store, session, account, &sample, now)?;
        }
        // A refusal at a limit is swapped by `refused_at_limit`, with its own reason.
        let unified = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-ratelimit-unified-status"))
            .map(|(_, v)| v.trim());
        if relay_proto::is_limit_refusal(status, unified) {
            return Ok(None);
        }
        threshold_swap(&store, account, now)
    }

    /// Runs a swap in the background, on a thread that may block.
    fn start_swap(&self, req: SwapRequest) {
        let swaps = self.swaps.clone();
        let log = self.log.clone();
        let job = move || {
            let from = req.from.map_or("-".to_string(), |a| a.to_string());
            let reason = req.reason.as_str();
            log.log(&match swaps.run(&req) {
                Ok(outcome) => format!("swap from {from} ({reason}): {outcome:?}"),
                Err(e) => format!("swap from {from} ({reason}) failed: {e:#}"),
            });
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(job);
            }
            Err(_) => {
                let _ = std::thread::Builder::new()
                    .name("relay-swap".into())
                    .spawn(job);
            }
        }
    }
}

impl StoreEvents {
    /// Swaps away from `account` if it is still the active one and says which account is active
    /// now: the swap at a refused limit (spec 8) or at a refused login (spec 9).
    /// With relay mode off, `hold` and the swap are skipped and the refusal goes back as it is.
    fn swap_away(
        &self,
        account: i64,
        reason: Reason,
        why: &str,
        hold: impl FnOnce(&Store),
    ) -> Swapped {
        let _one = self.refusals.lock().unwrap_or_else(|e| e.into_inner());
        {
            let store = self.store();
            if !self.mode_on(&store) {
                self.log(&format!(
                    "account={account} refused {why}; the app account is off, so no swap"
                ));
                return Swapped::No;
            }
            hold(&store);
        }
        // Another refusal's swap may have moved the pool on already: then there is nothing to
        // swap, and the request goes to the account that is active now.
        if self.active() == Some(account) {
            let req = SwapRequest {
                from: Some(account),
                to: None,
                reason,
                wait: true,
            };
            self.log(&match self.swaps.run(&req) {
                Ok(outcome) => format!("swap from {account} {why}: {outcome:?}"),
                Err(e) => format!("swap from {account} {why} failed: {e:#}"),
            });
        }
        // Whatever the swap did, the account active now is the one to try.
        match self.active() {
            Some(active) if active != account => Swapped::To(active),
            _ => Swapped::No,
        }
    }
}

/// The swap the status line would ask for after a reading of `account` (main 6), when it is the
/// active account: a threshold, or a better account available.
/// Asking is recorded, so readings that follow within the request interval ask for none.
fn threshold_swap(store: &Store, account: i64, now_ms: i64) -> Result<Option<SwapRequest>> {
    if store.active_account()?.map(|a| a.id) != Some(account) {
        return Ok(None);
    }
    let (pool, policy) = swap::load_pool(store, now_ms / 1000)?;
    let timing = statusline::timing(store, account, now_ms)?;
    let Some(reason) = scheduler::decide(&pool, policy, timing, false, true) else {
        return Ok(None);
    };
    store.set_meta(statusline::META_SWAP_REQUESTED_AT, now_ms)?;
    Ok(Some(SwapRequest {
        from: Some(account),
        to: None,
        reason,
        wait: false,
    }))
}

impl RelayEvents for StoreEvents {
    fn response(
        &self,
        account: i64,
        session: Option<&str>,
        model: Option<&str>,
        status: u16,
        headers: &[(String, String)],
    ) {
        match self.record(account, session, model, status, headers) {
            Ok(Some(req)) => self.start_swap(req),
            Ok(None) => {}
            Err(e) => self.log(&format!(
                "recording a reading of account {account} failed: {e:#}"
            )),
        }
    }

    fn log(&self, line: &str) {
        self.log.log(line);
    }

    fn refused_at_limit(&self, account: i64) -> Swapped {
        self.swap_away(account, Reason::LimitRefused, "at a refused limit", |_| {})
    }

    fn dead_login(&self, account: i64) -> Swapped {
        self.swap_away(
            account,
            Reason::Unauthorized,
            "at a refused login",
            |store| {
                // Until a check settles it, no swap picks the account again.
                if let Err(e) = usage_check::hold_back(store, account) {
                    self.log(&format!("holding back account {account} failed: {e:#}"));
                }
            },
        )
    }

    fn resent(&self, from: i64, to: i64) {
        if let Err(e) = self
            .store()
            .log_event(kind::RELAY_RESEND, Some(from), Some(to), None, None)
        {
            self.log(&format!("recording a resend failed: {e:#}"));
        }
    }

    fn unreachable(&self, failure: &str) {
        let now = time::now_ms();
        {
            let mut last = self.last_error_ms.lock().unwrap_or_else(|e| e.into_inner());
            if now - *last < ERROR_EVERY_MS {
                return;
            }
            *last = now;
        }
        if let Err(e) = self.store().log_event(
            kind::RELAY_ERROR,
            None,
            None,
            None,
            Some(json!({ "kind": failure })),
        ) {
            self.log(&format!("recording an error failed: {e:#}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use ccr_core::scheduler::Reason;
    use ccr_core::store::{AccountIdentity, AccountState, WaitingState, kind};

    use super::*;

    type Act = Box<dyn Fn(&Store, &SwapRequest) -> Result<SwapOutcome> + Send + Sync>;

    /// Swaps that record each request and do what the test says on a store of their own.
    struct FakeSwaps {
        db: std::path::PathBuf,
        tx: Mutex<mpsc::Sender<SwapRequest>>,
        act: Act,
    }

    impl Swaps for FakeSwaps {
        fn run(&self, req: &SwapRequest) -> Result<SwapOutcome> {
            let _ = self.tx.lock().unwrap().send(req.clone());
            let store = Store::open(&self.db)?;
            (self.act)(&store, req)
        }
    }

    /// A store file with `b` active, `c` parked and home account `a` parked, in relay mode, and
    /// events over it.
    struct Env {
        _root: tempfile::TempDir,
        db: std::path::PathBuf,
        b: i64,
        c: i64,
        events: StoreEvents,
        swaps: mpsc::Receiver<SwapRequest>,
    }

    fn env(act: Act) -> Env {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("store.db");
        let store = Store::open(&db).unwrap();
        let b = store
            .insert_account("b", &AccountIdentity::default(), AccountState::Active)
            .unwrap();
        let c = store
            .insert_account("c", &AccountIdentity::default(), AccountState::Parked)
            .unwrap();
        let a = store
            .insert_account("a", &AccountIdentity::default(), AccountState::Parked)
            .unwrap();
        ccr_core::relay::set_mode(&store, Some(a)).unwrap();
        let (tx, rx) = mpsc::channel();
        let swaps = Arc::new(FakeSwaps {
            db: db.clone(),
            tx: Mutex::new(tx),
            act,
        });
        let events = StoreEvents::new(store, swaps, Arc::new(Quiet));
        Env {
            _root: root,
            db,
            b,
            c,
            events,
            swaps: rx,
        }
    }

    struct Quiet;
    impl RelayEvents for Quiet {}

    /// Makes `to` the active account and `from` parked.
    fn move_to(store: &Store, from: i64, to: i64) {
        store.set_state(from, AccountState::Parked).unwrap();
        store.set_state(to, AccountState::Active).unwrap();
    }

    /// Swaps to the first parked account.
    fn swap_to_parked() -> Act {
        Box::new(|store, req| {
            let from = req.from.unwrap();
            let to = store
                .accounts()?
                .into_iter()
                .find(|a| a.state == AccountState::Parked)
                .unwrap()
                .id;
            move_to(store, from, to);
            Ok(SwapOutcome::Swapped {
                from: Some(from),
                to,
            })
        })
    }

    fn exhausted() -> Act {
        Box::new(|_, _| Ok(SwapOutcome::PoolExhausted))
    }

    fn headers(pct_5h: &str, status: &str) -> Vec<(String, String)> {
        let reset = (ccr_core::time::now_s() + 3600).to_string();
        [
            ("anthropic-ratelimit-unified-status", status.to_string()),
            ("anthropic-ratelimit-unified-5h-utilization", pct_5h.into()),
            ("anthropic-ratelimit-unified-5h-reset", reset),
            ("request-id", "req_1".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    impl Env {
        fn open(&self) -> Store {
            Store::open(&self.db).unwrap()
        }

        fn next_swap(&self) -> Option<SwapRequest> {
            self.swaps.recv_timeout(Duration::from_secs(5)).ok()
        }

        fn no_swap(&self) {
            let got = self.swaps.recv_timeout(Duration::from_millis(300));
            assert!(got.is_err(), "unexpected swap {got:?}");
        }
    }

    #[test]
    fn response_records_the_reading_for_its_account() {
        let env = env(exhausted());
        env.events.response(
            env.c,
            Some("sess-1"),
            Some("claude-x"),
            200,
            &headers("0.4", "allowed"),
        );
        let s = env.open().last_sample(env.c).unwrap().unwrap();
        assert_eq!(s.profile, "relay");
        assert_eq!(s.session_id.as_deref(), Some("sess-1"));
        assert_eq!(s.model.as_deref(), Some("claude-x"));
        assert_eq!(s.windows.len(), 1);
        assert_eq!(s.windows[0].name, "five_hour");
        assert!((s.windows[0].pct - 40.0).abs() < 1e-9);
        assert!(env.open().last_sample(env.b).unwrap().is_none());
    }

    #[test]
    fn response_without_readings_records_nothing() {
        let env = env(exhausted());
        env.events
            .response(env.b, None, None, 200, &[("x".into(), "y".into())]);
        assert!(env.open().last_sample(env.b).unwrap().is_none());
        env.no_swap();
    }

    #[test]
    fn threshold_on_the_active_account_swaps() {
        let env = env(exhausted());
        env.events
            .response(env.b, None, None, 200, &headers("0.995", "allowed_warning"));
        let req = env.next_swap().expect("a threshold swap");
        assert_eq!(req.from, Some(env.b));
        assert_eq!(req.to, None);
        assert_eq!(req.reason, Reason::FiveHour);
        assert!(!req.wait);

        // Asked for once; the next reading within the request interval asks for none.
        env.events
            .response(env.b, None, None, 200, &headers("0.996", "allowed_warning"));
        env.no_swap();
    }

    #[test]
    fn no_threshold_swap_for_another_account_or_a_refusal() {
        let env = env(exhausted());
        // `c` is not the active account.
        env.events
            .response(env.c, None, None, 200, &headers("0.995", "allowed_warning"));
        env.no_swap();
        // A refusal at a limit is swapped by `refused_at_limit`, with its own reason.
        env.events
            .response(env.b, None, None, 429, &headers("1.0", "rejected"));
        env.no_swap();
        assert!(env.open().last_sample(env.b).unwrap().is_some());
    }

    #[test]
    fn below_the_threshold_no_swap() {
        let env = env(exhausted());
        env.events
            .response(env.b, None, None, 200, &headers("0.5", "allowed"));
        env.no_swap();
    }

    #[test]
    fn reading_resumes_a_waiting_session() {
        let env = env(exhausted());
        let since = ccr_core::time::now_ms() - 60_000;
        env.open()
            .start_waiting("sess-1", since, Some(env.b), None)
            .unwrap();
        env.events
            .response(env.b, Some("sess-1"), None, 200, &headers("0.3", "allowed"));
        let ws = env.open().waiting_session("sess-1").unwrap().unwrap();
        assert_eq!(ws.state, WaitingState::Resumed);
    }

    #[test]
    fn refused_at_limit_swaps_and_names_the_new_account() {
        let env = env(swap_to_parked());
        assert_eq!(env.events.refused_at_limit(env.b), Swapped::To(env.c));
    }

    #[test]
    fn refused_at_limit_asks_for_the_swap_it_needs() {
        let env = env(exhausted());
        assert_eq!(env.events.refused_at_limit(env.b), Swapped::No);
        let req = env.next_swap().unwrap();
        assert_eq!(req.from, Some(env.b));
        assert_eq!(req.to, None);
        assert_eq!(req.reason, Reason::LimitRefused);
        assert!(req.wait);
    }

    #[test]
    fn refused_at_limit_after_another_swap_moved_on() {
        // The swap is skipped because another one already moved the pool on.
        let env = env(Box::new(|_, _| {
            Ok(SwapOutcome::Skipped {
                why: "the account in use already changed".into(),
            })
        }));
        move_to(&env.open(), env.b, env.c);
        assert_eq!(env.events.refused_at_limit(env.b), Swapped::To(env.c));
        // No swap was needed: `b` was no longer the active account.
        env.no_swap();
    }

    #[test]
    fn refused_at_limit_reads_the_active_account_after_a_failed_swap() {
        let swap = swap_to_parked();
        let env = env(Box::new(move |store, req| {
            swap(store, req)?;
            anyhow::bail!("the swap failed after the switch")
        }));
        assert_eq!(env.events.refused_at_limit(env.b), Swapped::To(env.c));
    }

    #[test]
    fn refused_at_limit_without_an_active_account() {
        let env = env(exhausted());
        env.open().set_state(env.b, AccountState::Parked).unwrap();
        assert_eq!(env.events.refused_at_limit(env.b), Swapped::No);
    }

    #[test]
    fn dead_login_holds_back_and_swaps() {
        let env = env(swap_to_parked());
        assert_eq!(env.events.dead_login(env.b), Swapped::To(env.c));
        let req = env.next_swap().unwrap();
        assert_eq!(req.from, Some(env.b));
        assert_eq!(req.to, None);
        assert_eq!(req.reason, Reason::Unauthorized);
        assert!(req.wait);
        let st = ccr_core::usage_check::state(&env.open(), env.b).unwrap();
        assert!(st.held_back);
        assert!(
            !ccr_core::usage_check::state(&env.open(), env.c)
                .unwrap()
                .held_back
        );
    }

    #[test]
    fn dead_login_after_another_swap_moved_on() {
        let env = env(exhausted());
        move_to(&env.open(), env.b, env.c);
        assert_eq!(env.events.dead_login(env.b), Swapped::To(env.c));
        env.no_swap();
        assert!(
            ccr_core::usage_check::state(&env.open(), env.b)
                .unwrap()
                .held_back
        );
    }

    /// With relay mode off the relay only forwards: the status line records readings and asks
    /// for swaps, and a swap writes the profile's login, which the sessions use.
    fn relay_off(env: &Env) {
        ccr_core::relay::set_mode(&env.open(), None).unwrap();
    }

    #[test]
    fn relay_off_records_no_reading_and_asks_for_no_swap() {
        let env = env(swap_to_parked());
        relay_off(&env);
        env.events.response(
            env.b,
            Some("sess-1"),
            None,
            200,
            &headers("0.995", "allowed_warning"),
        );
        env.events.response(
            env.b,
            Some("sess-1"),
            None,
            429,
            &headers("1.0", "rejected"),
        );
        env.no_swap();
        assert!(env.open().last_sample(env.b).unwrap().is_none());
    }

    #[test]
    fn relay_off_refused_at_limit_swaps_nothing() {
        let env = env(swap_to_parked());
        relay_off(&env);
        assert_eq!(env.events.refused_at_limit(env.b), Swapped::No);
        env.no_swap();
        assert_eq!(
            env.open().active_account().unwrap().map(|a| a.id),
            Some(env.b)
        );
    }

    #[test]
    fn relay_off_dead_login_holds_nothing_back() {
        let env = env(swap_to_parked());
        relay_off(&env);
        assert_eq!(env.events.dead_login(env.b), Swapped::No);
        env.no_swap();
        assert!(
            !ccr_core::usage_check::state(&env.open(), env.b)
                .unwrap()
                .held_back
        );
    }

    #[test]
    fn resent_is_an_event() {
        let env = env(exhausted());
        env.events.resent(env.b, env.c);
        let events = env
            .open()
            .events_of_kind_since(kind::RELAY_RESEND, 0)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].account_from, Some(env.b));
        assert_eq!(events[0].account_to, Some(env.c));
    }

    #[test]
    fn unreachable_at_most_once_a_minute() {
        let env = env(exhausted());
        env.events.unreachable("connect");
        env.events.unreachable("connect");
        let count = || {
            env.open()
                .events_of_kind_since(kind::RELAY_ERROR, 0)
                .unwrap()
                .len()
        };
        assert_eq!(count(), 1);
        // A minute later, another.
        *env.events.last_error_ms.lock().unwrap() -= 61_000;
        env.events.unreachable("other");
        assert_eq!(count(), 2);
        let last = env
            .open()
            .last_event_of(kind::RELAY_ERROR)
            .unwrap()
            .unwrap();
        assert_eq!(last.detail, Some(serde_json::json!({"kind": "other"})));
    }
}
