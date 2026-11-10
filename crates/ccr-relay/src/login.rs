//! The login a model request goes out with, from the store (spec 5.1).

use std::sync::Mutex;

use anyhow::{Context, Result};
use ccr_core::Store;
use serde_json::Value;
use zeroize::{Zeroize, Zeroizing};

use crate::{Choice, Logins};

/// Reads which account is active from the store on every call, so a swap reaches the next
/// request of every session.
/// The active account's access token is decrypted only when its stored login changes, held in
/// memory, and zeroed when replaced or no longer needed.
pub struct StoreLogins {
    inner: Mutex<Inner>,
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

impl StoreLogins {
    pub fn new(store: Store) -> StoreLogins {
        StoreLogins {
            inner: Mutex::new(Inner {
                store,
                cached: None,
            }),
        }
    }
}

impl Logins for StoreLogins {
    fn for_model(&self) -> Result<Choice> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *guard;
        let store = &inner.store;
        let active = store.active_account()?.context("no account is active")?;
        // The profile is signed in to the home account (or, with relay mode off, to the active
        // one): the session's own login is the right one.
        let profile = ccr_core::relay::profile_account(store)?;
        if profile.is_some_and(|p| p.id == active.id) {
            inner.cached = None;
            return Ok(Choice::PassThrough { account: active.id });
        }
        let meta = store
            .credential_meta(active.id)?
            .with_context(|| format!("account {} has no stored login", active.id))?;
        if let Some(c) = &inner.cached
            && c.account == active.id
            && c.refresh_fp == meta.refresh_fp
            && c.updated_at == meta.updated_at
        {
            return Ok(Choice::Token {
                account: active.id,
                token: c.token.clone(),
            });
        }
        inner.cached = None;
        let mut cred = store
            .credential(active.id)?
            .with_context(|| format!("account {} has no stored login", active.id))?;
        let token = cred
            .oauth
            .get("accessToken")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(|t| Zeroizing::new(t.to_string()));
        wipe(&mut cred.oauth);
        let token = token
            .with_context(|| format!("account {}'s stored login has no access token", active.id))?;
        inner.cached = Some(Cached {
            account: active.id,
            refresh_fp: cred.refresh_fp,
            updated_at: cred.updated_at,
            token: token.clone(),
        });
        Ok(Choice::Token {
            account: active.id,
            token,
        })
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
    use ccr_core::store::{AccountIdentity, AccountState};
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
        let logins = StoreLogins::new(store);
        assert!(matches!(
            logins.for_model().unwrap(),
            Choice::PassThrough { account } if account == a
        ));
    }

    #[test]
    fn login_relay_mode_off_passes_through() {
        let (store, _, b) = store();
        ccr_core::relay::set_mode(&store, None).unwrap();
        let logins = StoreLogins::new(store);
        assert!(matches!(
            logins.for_model().unwrap(),
            Choice::PassThrough { account } if account == b
        ));
    }

    #[test]
    fn login_other_active_gets_its_token() {
        let (store, _, b) = store();
        let logins = StoreLogins::new(store);
        assert_eq!(token_of(logins.for_model().unwrap()), (b, "tok-b".into()));
    }

    #[test]
    fn login_decrypts_only_when_the_stored_login_changes() {
        let (store, _, b) = store();
        let logins = StoreLogins::new(store);
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
        let logins = StoreLogins::new(store);
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
        let logins = StoreLogins::new(store);
        assert!(logins.for_model().is_err());

        let logins = StoreLogins::new(store_without_login());
        let err = logins.for_model().unwrap_err().to_string();
        assert!(err.contains("no stored login"), "{err}");
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
