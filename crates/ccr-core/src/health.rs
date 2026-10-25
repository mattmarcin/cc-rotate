//! Things that need the user's attention before they break an unattended run (spec section
//! 21): sign-ins about to end, sessions that did not continue after a limit (`waiting`), and the
//! Claude Code version the profile's sessions run. `scan` turns each into an event once; the
//! desktop app shows the ones worth a notification.

use anyhow::Result;
use serde_json::json;

use crate::doctor::TESTED_CLAUDE_VERSIONS;
use crate::store::{AccountState, Store, kind};
use crate::waiting;

/// The Claude Code version the rotation profile's status line last reported.
pub const META_CLAUDE_VERSION: &str = "claude_version_seen";
/// When `scan` last ran from a status line tick.
const META_SCAN_AT: &str = "health_scan_at";
/// Status line ticks run every 30 seconds in every session; one scan a minute is plenty.
const TICK_SCAN_EVERY_MS: i64 = 60_000;

const DAY_MS: i64 = 86_400_000;

/// Check everything once. The desktop app calls this every 30 seconds.
pub fn scan(store: &Store, now_ms: i64) -> Result<()> {
    waiting::scan(store, now_ms)?;
    sign_ins(store, now_ms)
}

/// `scan`, at most once a minute across every session's status line, so a machine without
/// the desktop app (a server over SSH) still records what needs attention.
pub fn scan_from_tick(store: &Store, now_ms: i64) -> Result<()> {
    let due = store.immediate(|s| {
        let due = s
            .meta_i64(META_SCAN_AT)?
            .is_none_or(|t| now_ms - t >= TICK_SCAN_EVERY_MS || t > now_ms);
        if due {
            s.set_meta(META_SCAN_AT, now_ms)?;
        }
        Ok(due)
    })?;
    if due {
        scan(store, now_ms)?;
    }
    Ok(())
}

/// When an account's sign-in ends (epoch ms): Claude Code can no longer renew its login after
/// that, whatever its use, and the account needs a new sign-in.
pub fn sign_in_ends(store: &Store, account_id: i64) -> Result<Option<i64>> {
    Ok(store
        .credential_meta(account_id)?
        .and_then(|m| m.refresh_expires_at))
}

/// Whether a sign-in ending at `ends_ms` is close enough to warn about.
pub fn sign_in_ends_soon(ends_ms: i64, warning_days: i64, now_ms: i64) -> bool {
    warning_days > 0 && ends_ms > now_ms && ends_ms - now_ms <= warning_days * DAY_MS
}

/// Warn once per sign-in that ends within `login_warning_days`, and mark an account not in use
/// whose sign-in ended (and whose access token expired with it) as signed out, so rotation
/// never swaps to a login that cannot work. The account in use is left to its sessions: Claude
/// Code's renewal fails and the status line reports the dead login (spec section 8).
fn sign_ins(store: &Store, now_ms: i64) -> Result<()> {
    let days = store.settings()?.login_warning_days;
    for account in store.accounts()? {
        if matches!(account.state, AccountState::NeedsLogin) {
            continue;
        }
        let Some(meta) = store.credential_meta(account.id)? else {
            continue;
        };
        let Some(ends) = meta.refresh_expires_at else {
            continue;
        };
        if ends <= now_ms {
            let unusable = meta.expires_at.is_none_or(|t| t <= now_ms);
            if unusable && matches!(account.state, AccountState::Parked | AccountState::Paused) {
                store.immediate(|s| {
                    // Re-read under the write lock: a new sign-in may have landed meanwhile.
                    let current = s.credential_meta(account.id)?;
                    if current.and_then(|m| m.refresh_expires_at) != Some(ends) {
                        return Ok(());
                    }
                    s.set_state(account.id, AccountState::NeedsLogin)?;
                    s.log_event(
                        kind::DEAD_LOGIN,
                        Some(account.id),
                        None,
                        Some("sign_in_ended"),
                        Some(json!({ "label": account.label, "expires_at": ends })),
                    )?;
                    Ok(())
                })?;
            }
            continue;
        }
        if !sign_in_ends_soon(ends, days, now_ms) {
            continue;
        }
        let key = format!("login_expiry_warned:{}", account.id);
        store.immediate(|s| {
            if s.meta_i64(&key)? == Some(ends) {
                return Ok(());
            }
            s.set_meta(&key, ends)?;
            s.log_event(
                kind::LOGIN_EXPIRING,
                Some(account.id),
                None,
                None,
                Some(json!({ "label": account.label, "expires_at": ends })),
            )?;
            Ok(())
        })?;
    }
    Ok(())
}

/// The Claude Code version a status line reported. A version not seen before is recorded with
/// an event, saying whether this release of cc-rotate was tested with it.
pub fn note_claude_version(store: &Store, version: &str) -> Result<()> {
    let version = version.trim();
    if version.is_empty() || version.len() > 40 {
        return Ok(());
    }
    if store.meta(META_CLAUDE_VERSION)?.as_deref() == Some(version) {
        return Ok(());
    }
    store.immediate(|s| {
        let previous = s.meta(META_CLAUDE_VERSION)?;
        if previous.as_deref() == Some(version) {
            return Ok(());
        }
        s.set_meta(META_CLAUDE_VERSION, version)?;
        s.log_event(
            kind::CLAUDE_VERSION,
            None,
            None,
            None,
            Some(json!({
                "version": version,
                "previous": previous,
                "tested": is_tested(version),
            })),
        )?;
        Ok(())
    })
}

pub fn is_tested(version: &str) -> bool {
    TESTED_CLAUDE_VERSIONS.contains(&version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::AccountIdentity;
    use serde_json::Value;

    fn store_with(state: AccountState, ends: i64, access_expires: i64) -> (Store, i64) {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .insert_account("main", &AccountIdentity::default(), state)
            .unwrap();
        store
            .save_credential(
                id,
                &json!({
                    "accessToken": "a", "refreshToken": "r",
                    "expiresAt": access_expires, "refreshTokenExpiresAt": ends,
                }),
                "login",
            )
            .unwrap();
        (store, id)
    }

    fn events(store: &Store, kind: &str) -> Vec<crate::store::Event> {
        store
            .events(100)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind)
            .collect()
    }

    #[test]
    fn warns_once_per_sign_in_that_ends_soon() {
        let now = 1_000 * DAY_MS;
        let (store, id) = store_with(AccountState::Parked, now + 4 * DAY_MS, now + 3_600_000);
        scan(&store, now).unwrap();
        scan(&store, now + 60_000).unwrap();
        let warned = events(&store, kind::LOGIN_EXPIRING);
        assert_eq!(warned.len(), 1);
        assert_eq!(warned[0].account_from, Some(id));
        assert_eq!(
            warned[0].detail.as_ref().unwrap()["expires_at"],
            Value::from(now + 4 * DAY_MS)
        );

        // A new sign-in ends later: no warning until that one is close too.
        store
            .save_credential(
                id,
                &json!({"accessToken": "b", "refreshToken": "s",
                        "expiresAt": now + 3_600_000, "refreshTokenExpiresAt": now + 30 * DAY_MS}),
                "login",
            )
            .unwrap();
        scan(&store, now + 120_000).unwrap();
        assert_eq!(events(&store, kind::LOGIN_EXPIRING).len(), 1);
        scan(&store, now + 26 * DAY_MS).unwrap();
        assert_eq!(events(&store, kind::LOGIN_EXPIRING).len(), 2);
    }

    #[test]
    fn no_warning_far_ahead_or_when_turned_off() {
        let now = 1_000 * DAY_MS;
        let (store, _) = store_with(AccountState::Parked, now + 6 * DAY_MS, now + 3_600_000);
        scan(&store, now).unwrap();
        assert!(events(&store, kind::LOGIN_EXPIRING).is_empty());

        store.set_setting("login_warning_days", Some("0")).unwrap();
        scan(&store, now + 5 * DAY_MS).unwrap();
        assert!(events(&store, kind::LOGIN_EXPIRING).is_empty());
        assert!(store.set_setting("login_warning_days", Some("31")).is_err());
    }

    #[test]
    fn an_ended_sign_in_signs_out_an_account_not_in_use() {
        let now = 1_000 * DAY_MS;
        let (store, id) = store_with(AccountState::Parked, now - 1, now - 1);
        scan(&store, now).unwrap();
        assert_eq!(
            store.account(id).unwrap().unwrap().state,
            AccountState::NeedsLogin
        );
        let dead = events(&store, kind::DEAD_LOGIN);
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].reason.as_deref(), Some("sign_in_ended"));
        scan(&store, now + 60_000).unwrap();
        assert_eq!(events(&store, kind::DEAD_LOGIN).len(), 1);
    }

    #[test]
    fn an_ended_sign_in_waits_for_its_access_token_and_spares_the_account_in_use() {
        let now = 1_000 * DAY_MS;
        // Its access token still works for an hour.
        let (store, id) = store_with(AccountState::Parked, now - 1, now + 3_600_000);
        scan(&store, now).unwrap();
        assert_eq!(
            store.account(id).unwrap().unwrap().state,
            AccountState::Parked
        );

        let (store, id) = store_with(AccountState::Active, now - 1, now - 1);
        scan(&store, now).unwrap();
        assert_eq!(
            store.account(id).unwrap().unwrap().state,
            AccountState::Active
        );
    }

    #[test]
    fn records_each_new_claude_code_version_once() {
        let store = Store::open_in_memory().unwrap();
        let tested = TESTED_CLAUDE_VERSIONS[0];
        note_claude_version(&store, tested).unwrap();
        note_claude_version(&store, tested).unwrap();
        note_claude_version(&store, "9.9.999").unwrap();
        let seen = events(&store, kind::CLAUDE_VERSION);
        assert_eq!(seen.len(), 2);
        // Newest first.
        let newer = seen[0].detail.as_ref().unwrap();
        assert_eq!(newer["version"], "9.9.999");
        assert_eq!(newer["previous"], tested);
        assert_eq!(newer["tested"], false);
        assert_eq!(seen[1].detail.as_ref().unwrap()["tested"], true);
        note_claude_version(&store, "").unwrap();
        assert_eq!(events(&store, kind::CLAUDE_VERSION).len(), 2);
    }

    #[test]
    fn tick_scans_at_most_once_a_minute() {
        let now = 1_000 * DAY_MS;
        let (store, _) = store_with(AccountState::Parked, now + DAY_MS, now + 3_600_000);
        store.set_setting("login_warning_days", Some("0")).unwrap();
        scan_from_tick(&store, now).unwrap();
        store.set_setting("login_warning_days", Some("5")).unwrap();
        scan_from_tick(&store, now + 30_000).unwrap();
        assert!(events(&store, kind::LOGIN_EXPIRING).is_empty());
        scan_from_tick(&store, now + 61_000).unwrap();
        assert_eq!(events(&store, kind::LOGIN_EXPIRING).len(), 1);
    }
}
