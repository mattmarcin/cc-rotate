//! The login a model request goes out with, from the store (spec 5.1), and its renewal through
//! Claude Code (spec 9).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ccr_core::store::CredentialMeta;
use ccr_core::usage_check::{self, CheckOutcome};
use ccr_core::{Paths, Store, relay_proto, time};
use serde_json::Value;
use zeroize::{Zeroize, Zeroizing};

use crate::{Choice, Logins, RelayEvents};

/// A renewal that ended without moving the login's expiry out of the margin is not started again
/// by requests for this long; one asked for after a 401 always is.
const RENEW_AGAIN_AFTER: Duration = Duration::from_secs(60);

/// The largest `relay_renew_margin_s` the settings allow: a login that expires later than this
/// is never due, and the settings are not read for it.
const MAX_MARGIN_MS: i64 = 3_600_000;

/// Renews an account's stored login: a usage check that makes Claude Code renew it in a scratch
/// profile (spec 9). cc-rotate never calls a sign-in endpoint itself.
/// It runs Claude Code and waits for the account's lock, so it is only called on a thread that
/// may block.
pub trait Renewals: Send + Sync {
    fn renew(&self, account: i64) -> Result<CheckOutcome>;
}

/// `usage_check::renew` on a store of its own, opened for each renewal.
pub struct StoreRenewals {
    paths: Paths,
    log: Arc<dyn RelayEvents>,
}

impl StoreRenewals {
    pub fn new(paths: Paths, log: Arc<dyn RelayEvents>) -> StoreRenewals {
        StoreRenewals { paths, log }
    }
}

impl Renewals for StoreRenewals {
    fn renew(&self, account: i64) -> Result<CheckOutcome> {
        let outcome = Store::open(&self.paths.store_db())
            .and_then(|store| usage_check::renew(&store, &self.paths, account));
        self.log.log(&match &outcome {
            Ok(outcome) => format!("account={account} renewal: {}", outcome.kind()),
            Err(e) => format!("account={account} renewal failed: {e:#}"),
        });
        outcome
    }
}

/// Reads which account is active from the store on every call, so a swap reaches the next
/// request of every session.
/// The active account's access token is decrypted only when its stored login changes, held in
/// memory, and zeroed when replaced or no longer needed.
/// When it is due to expire within `relay_renew_margin_s`, a renewal starts in the background.
pub struct StoreLogins {
    inner: Mutex<Inner>,
    renewals: Arc<dyn Renewals>,
    renewing: Arc<Mutex<Renewing>>,
}

struct Inner {
    store: Store,
    cached: Option<Cached>,
}

/// A decrypted token and the stored login it came from.
struct Cached {
    account: i64,
    refresh_fp: String,
    updated_at: i64,
    token: Zeroizing<String>,
}

/// The renewals running, at most one per account, and when each account's last one ended.
#[derive(Default)]
struct Renewing {
    running: HashSet<i64>,
    ended: HashMap<i64, Instant>,
}

/// Marks a renewal ended when its job finishes, fails to start, or panics.
struct Ended {
    renewing: Arc<Mutex<Renewing>>,
    account: i64,
}

impl Drop for Ended {
    fn drop(&mut self) {
        let mut r = self.renewing.lock().unwrap_or_else(|e| e.into_inner());
        r.running.remove(&self.account);
        r.ended.insert(self.account, Instant::now());
    }
}

impl StoreLogins {
    pub fn new(store: Store, renewals: Arc<dyn Renewals>) -> StoreLogins {
        StoreLogins {
            inner: Mutex::new(Inner {
                store,
                cached: None,
            }),
            renewals,
            renewing: Arc::new(Mutex::new(Renewing::default())),
        }
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start a renewal of `account` on a thread that may block, unless one is running.
    /// A `routine` one, asked for by a request, also waits `RENEW_AGAIN_AFTER` after the last.
    fn start_renewal(&self, account: i64, routine: bool) {
        {
            let mut r = self.renewing.lock().unwrap_or_else(|e| e.into_inner());
            if r.running.contains(&account)
                || (routine
                    && r.ended
                        .get(&account)
                        .is_some_and(|t| t.elapsed() < RENEW_AGAIN_AFTER))
            {
                return;
            }
            r.running.insert(account);
        }
        let ended = Ended {
            renewing: self.renewing.clone(),
            account,
        };
        let renewals = self.renewals.clone();
        let job = move || {
            let _ended = ended;
            // `StoreRenewals` logs what it did.
            let _ = renewals.renew(account);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(job);
            }
            Err(_) => {
                // A thread that does not start drops the job, which ends the renewal.
                let _ = std::thread::Builder::new()
                    .name("relay-renew".into())
                    .spawn(job);
            }
        }
    }
}

impl Inner {
    fn meta(&self, account: i64) -> Result<CredentialMeta> {
        self.store
            .credential_meta(account)?
            .with_context(|| format!("account {account} has no stored login"))
    }

    /// `account`'s access token, decrypted only when its stored login differs from the one in
    /// memory.
    fn token(&mut self, account: i64, meta: &CredentialMeta) -> Result<Zeroizing<String>> {
        if let Some(c) = &self.cached
            && c.account == account
            && c.refresh_fp == meta.refresh_fp
            && c.updated_at == meta.updated_at
        {
            return Ok(c.token.clone());
        }
        self.cached = None;
        let mut cred = self
            .store
            .credential(account)?
            .with_context(|| format!("account {account} has no stored login"))?;
        let token = cred
            .oauth
            .get("accessToken")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(|t| Zeroizing::new(t.to_string()));
        wipe(&mut cred.oauth);
        let token = token
            .with_context(|| format!("account {account}'s stored login has no access token"))?;
        self.cached = Some(Cached {
            account,
            refresh_fp: cred.refresh_fp,
            updated_at: cred.updated_at,
            token: token.clone(),
        });
        Ok(token)
    }

    /// Whether the login that expires at `expires_at` (epoch ms) is due a renewal now.
    fn renew_due(&self, expires_at: Option<i64>) -> bool {
        let now = time::now_ms();
        match expires_at {
            Some(at) if at - now > MAX_MARGIN_MS => false,
            Some(at) => {
                let margin = self
                    .store
                    .settings()
                    .map_or(900, |s| s.relay_renew_margin_s);
                relay_proto::renew_due(at, now, margin)
            }
            // When is unknown: as a swap does, renew it.
            None => true,
        }
    }
}

impl Logins for StoreLogins {
    fn for_model(&self) -> Result<Choice> {
        let mut inner = self.inner();
        let active = inner
            .store
            .active_account()?
            .context("no account is active")?;
        // The profile is signed in to the home account (or, with relay mode off, to the active
        // one): the session's own login is the right one, and Claude Code renews it.
        let profile = ccr_core::relay::profile_account(&inner.store)?;
        if profile.is_some_and(|p| p.id == active.id) {
            inner.cached = None;
            return Ok(Choice::PassThrough { account: active.id });
        }
        let meta = inner.meta(active.id)?;
        let token = inner.token(active.id, &meta)?;
        // The token keeps working until it expires, so the request goes out with it meanwhile.
        if inner.renew_due(meta.expires_at) {
            self.start_renewal(active.id, true);
        }
        Ok(Choice::Token {
            account: active.id,
            token,
        })
    }

    fn renew(&self, account: i64) {
        let home = ccr_core::relay::profile_account(&self.inner().store);
        // When the profile's account cannot be read, renewing could retire the profile's login.
        if home.is_ok_and(|p| p.is_none_or(|p| p.id != account)) {
            self.start_renewal(account, false);
        }
    }

    fn refresh(&self, account: i64) -> Result<Choice> {
        let mut inner = self.inner();
        let profile = ccr_core::relay::profile_account(&inner.store)?;
        if profile.is_some_and(|p| p.id == account) {
            return Ok(Choice::PassThrough { account });
        }
        let meta = inner.meta(account)?;
        let token = inner.token(account, &meta)?;
        Ok(Choice::Token { account, token })
    }
}

/// Zero every string in a decrypted login before it is freed.
fn wipe(v: &mut Value) {
    match v {
        Value::String(s) => s.zeroize(),
        Value::Array(items) => items.iter_mut().for_each(wipe),
        Value::Object(map) => map.values_mut().for_each(wipe),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use ccr_core::store::{AccountIdentity, AccountState};
    use ccr_core::usage_check::CheckOutcome;
    use serde_json::json;

    /// A store with home account `a` (parked) and `b` (active), each with a stored login.
    fn store() -> (Store, i64, i64) {
        let store = Store::open_in_memory().unwrap();
        let a = store
            .insert_account("a", &AccountIdentity::default(), AccountState::Parked)
            .unwrap();
        let b = store
            .insert_account("b", &AccountIdentity::default(), AccountState::Active)
            .unwrap();
        for (id, tag) in [(a, "a"), (b, "b")] {
            store
                .save_credential(
                    id,
                    &json!({"accessToken": format!("tok-{tag}"), "refreshToken": format!("ref-{tag}"), "expiresAt": 10}),
                    "test",
                )
                .unwrap();
        }
        ccr_core::relay::set_mode(&store, Some(a)).unwrap();
        (store, a, b)
    }

    fn token_of(c: Choice) -> (i64, String) {
        match c {
            Choice::Token { account, token } => (account, token.to_string()),
            other => panic!("expected a token, got {other:?}"),
        }
    }

    #[test]
    fn login_home_active_passes_through() {
        let (store, a, b) = store();
        store.set_state(b, AccountState::Parked).unwrap();
        store.set_state(a, AccountState::Active).unwrap();
        let logins = StoreLogins::new(store, Counting::new());
        assert!(matches!(
            logins.for_model().unwrap(),
            Choice::PassThrough { account } if account == a
        ));
    }

    #[test]
    fn login_relay_mode_off_passes_through() {
        let (store, _, b) = store();
        ccr_core::relay::set_mode(&store, None).unwrap();
        let logins = StoreLogins::new(store, Counting::new());
        assert!(matches!(
            logins.for_model().unwrap(),
            Choice::PassThrough { account } if account == b
        ));
    }

    #[test]
    fn login_other_active_gets_its_token() {
        let (store, _, b) = store();
        let logins = StoreLogins::new(store, Counting::new());
        assert_eq!(token_of(logins.for_model().unwrap()), (b, "tok-b".into()));
    }

    #[test]
    fn login_decrypts_only_when_the_stored_login_changes() {
        let (store, _, b) = store();
        let logins = StoreLogins::new(store, Counting::new());
        assert_eq!(token_of(logins.for_model().unwrap()), (b, "tok-b".into()));

        // A blob that no longer decrypts proves the second call used the token in memory.
        {
            let inner = logins.inner.lock().unwrap();
            inner
                .store
                .conn()
                .execute_batch("UPDATE credentials SET oauth_blob = x'00'")
                .unwrap();
        }
        assert_eq!(token_of(logins.for_model().unwrap()), (b, "tok-b".into()));

        {
            let inner = logins.inner.lock().unwrap();
            inner
                .store
                .save_credential(
                    b,
                    &json!({"accessToken": "tok-b2", "refreshToken": "ref-b2", "expiresAt": 20}),
                    "test",
                )
                .unwrap();
        }
        assert_eq!(token_of(logins.for_model().unwrap()), (b, "tok-b2".into()));
    }

    #[test]
    fn login_follows_a_swap() {
        let (store, a, b) = store();
        let c = store
            .insert_account("c", &AccountIdentity::default(), AccountState::Parked)
            .unwrap();
        store
            .save_credential(
                c,
                &json!({"accessToken": "tok-c", "refreshToken": "ref-c", "expiresAt": 10}),
                "test",
            )
            .unwrap();
        let logins = StoreLogins::new(store, Counting::new());
        assert_eq!(token_of(logins.for_model().unwrap()), (b, "tok-b".into()));
        {
            let inner = logins.inner.lock().unwrap();
            inner.store.set_state(b, AccountState::Parked).unwrap();
            inner.store.set_state(c, AccountState::Active).unwrap();
        }
        assert_eq!(token_of(logins.for_model().unwrap()), (c, "tok-c".into()));
        {
            let inner = logins.inner.lock().unwrap();
            inner.store.set_state(c, AccountState::Parked).unwrap();
            inner.store.set_state(a, AccountState::Active).unwrap();
        }
        assert!(matches!(
            logins.for_model().unwrap(),
            Choice::PassThrough { account } if account == a
        ));
        assert!(logins.inner.lock().unwrap().cached.is_none());
    }

    #[test]
    fn login_errors_without_an_active_account_or_stored_login() {
        let (store, _, b) = store();
        store.set_state(b, AccountState::Parked).unwrap();
        let logins = StoreLogins::new(store, Counting::new());
        assert!(logins.for_model().is_err());

        let logins = StoreLogins::new(store_without_login(), Counting::new());
        let err = logins.for_model().unwrap_err().to_string();
        assert!(err.contains("no stored login"), "{err}");
    }

    /// Renewals that only count the accounts asked for.
    #[derive(Default)]
    struct Counting(Mutex<Vec<i64>>);

    impl Counting {
        fn new() -> Arc<Counting> {
            Arc::new(Counting::default())
        }
        fn calls(&self) -> Vec<i64> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Renewals for Counting {
        fn renew(&self, account: i64) -> Result<CheckOutcome> {
            self.0.lock().unwrap().push(account);
            Ok(CheckOutcome::Skipped {
                why: "a test".into(),
            })
        }
    }

    /// Waits up to 2 s for the renewals in the background to settle.
    fn settle(logins: &StoreLogins) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !logins.renewing.lock().unwrap().running.is_empty() {
            assert!(std::time::Instant::now() < until, "a renewal never ended");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn expiring(store: &Store, id: i64, token: &str, in_ms: i64) {
        store
            .save_credential(
                id,
                &json!({"accessToken": token, "refreshToken": format!("ref-{token}"), "expiresAt": ccr_core::time::now_ms() + in_ms}),
                "test",
            )
            .unwrap();
    }

    #[test]
    fn login_expiring_soon_starts_a_renewal() {
        let (store, _, b) = store();
        expiring(&store, b, "tok-b", 600_000);
        let renewals = Counting::new();
        let logins = StoreLogins::new(store, renewals.clone());
        assert_eq!(token_of(logins.for_model().unwrap()), (b, "tok-b".into()));
        settle(&logins);
        assert_eq!(renewals.calls(), vec![b]);
        // It just ended without a new login: not again at once.
        logins.for_model().unwrap();
        settle(&logins);
        assert_eq!(renewals.calls(), vec![b]);
    }

    #[test]
    fn login_with_time_left_starts_no_renewal() {
        let (store, _, b) = store();
        expiring(&store, b, "tok-b", 3_600_000);
        let renewals = Counting::new();
        let logins = StoreLogins::new(store, renewals.clone());
        logins.for_model().unwrap();
        settle(&logins);
        assert!(renewals.calls().is_empty());
    }

    #[test]
    fn renew_never_for_the_home_account() {
        let (store, a, b) = store();
        expiring(&store, a, "tok-a", 0);
        store.set_state(b, AccountState::Parked).unwrap();
        store.set_state(a, AccountState::Active).unwrap();
        let renewals = Counting::new();
        let logins = StoreLogins::new(store, renewals.clone());
        logins.for_model().unwrap();
        logins.renew(a);
        settle(&logins);
        assert!(renewals.calls().is_empty());
        // Asked for after a 401, it starts even though one ended just now.
        logins.renew(b);
        settle(&logins);
        logins.renew(b);
        settle(&logins);
        assert_eq!(renewals.calls(), vec![b, b]);
    }

    #[test]
    fn refresh_reads_the_stored_login_again() {
        let (store, a, b) = store();
        let logins = StoreLogins::new(store, Counting::new());
        assert_eq!(token_of(logins.for_model().unwrap()), (b, "tok-b".into()));
        assert_eq!(token_of(logins.refresh(b).unwrap()), (b, "tok-b".into()));
        {
            let inner = logins.inner.lock().unwrap();
            expiring(&inner.store, b, "tok-b2", 3_600_000);
        }
        assert_eq!(token_of(logins.refresh(b).unwrap()), (b, "tok-b2".into()));
        // The home account's login is the session's own.
        assert!(matches!(
            logins.refresh(a).unwrap(),
            Choice::PassThrough { account } if account == a
        ));
    }

    /// Home account `a` (parked) and `b` (active), neither with a stored login.
    fn store_without_login() -> Store {
        let store = Store::open_in_memory().unwrap();
        let a = store
            .insert_account("a", &AccountIdentity::default(), AccountState::Parked)
            .unwrap();
        store
            .insert_account("b", &AccountIdentity::default(), AccountState::Active)
            .unwrap();
        ccr_core::relay::set_mode(&store, Some(a)).unwrap();
        store
    }
}
