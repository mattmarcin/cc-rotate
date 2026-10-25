//! Usage the user's other Claude Code profiles already know (spec section 9.1).
//!
//! Every profile caches the last `/usage` reading Claude Code fetched for it in its
//! `.claude.json`, stamped with the account it was fetched for (`usage_report`). In a profile
//! whose account never changes that stamp is reliable, so when one of the user's other profiles
//! is signed in to an account in the pool, its reading counts as a reading of that account. This
//! is how cc-rotate learns that an account is used up because an agent ran in its own profile.
//!
//! Read-only, local, and cheap: a profile's `.claude.json` is only parsed again after it changed.
//! The rotation profile is never read this way, since a swap leaves its cache stamped with the
//! wrong account (F14).

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::paths::Paths;
use crate::store::{Sample, Store};
use crate::{credfile, profiles, usage_report};

/// A cached reading stamped more than this far in the future is not trusted.
const MAX_CLOCK_SKEW_MS: i64 = 5 * 60_000;

/// Prefix of the meta keys that remember each config file's modification time when it was
/// last read.
const META_SEEN_PREFIX: &str = "profile_usage_seen:";

/// Record the cached usage reading of every other profile that is signed in to a pool account
/// and has a reading newer than the store's latest for that account. Returns how many readings
/// were recorded. A profile that cannot be read is skipped, and tried again once it changes.
pub fn import(store: &Store, paths: &Paths, now_ms: i64) -> Result<usize> {
    let Ok(dirs) = profiles::profile_dirs(paths) else {
        // No home directory, or it cannot be listed: there is nothing to import.
        return Ok(0);
    };
    let mut recorded = 0;
    for dir in dirs {
        let config = profiles::config_file_for(&dir, &paths.home);
        let Some(mtime) = credfile::mtime_ms(&config) else {
            continue;
        };
        let key = format!("{META_SEEN_PREFIX}{}", config.display());
        if store.meta_i64(&key)? == Some(mtime) {
            continue;
        }
        let Ok(Some(doc)) = credfile::read_document(&config) else {
            continue;
        };
        if let Some(sample) = reading(store, &dir, &doc, now_ms)? {
            if store.record_sample(&sample)? {
                recorded += 1;
            }
        }
        store.set_meta(&key, mtime)?;
    }
    Ok(recorded)
}

/// The reading a profile's config holds for a pool account, if it is usable and new.
fn reading(
    store: &Store,
    dir: &Path,
    doc: &serde_json::Map<String, Value>,
    now_ms: i64,
) -> Result<Option<Sample>> {
    let signed_in_as = doc
        .get("oauthAccount")
        .and_then(|a| a.get("accountUuid"))
        .and_then(Value::as_str);
    let Some(report) = usage_report::from_config(doc) else {
        return Ok(None);
    };
    // Claude Code stamps the reading with the account the profile was signed in to when it
    // fetched it; one taken before a `/login` to another account must not count for this one.
    let (Some(uuid), Some(stamped)) = (signed_in_as, report.account_uuid.as_deref()) else {
        return Ok(None);
    };
    if uuid != stamped
        || report.windows.is_empty()
        || report.fetched_at_ms > now_ms + MAX_CLOCK_SKEW_MS
    {
        return Ok(None);
    }
    let Some(account) = store.account_by_uuid(uuid)? else {
        return Ok(None);
    };
    if store
        .last_sample(account.id)?
        .is_some_and(|last| last.ts >= report.fetched_at_ms)
    {
        return Ok(None);
    }
    Ok(Some(Sample {
        ts: report.fetched_at_ms,
        account_id: account.id,
        // Named like the readings observe mode records for the profile.
        profile: dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        session_id: None,
        model: None,
        windows: report.windows,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AccountIdentity, AccountState, SampleWindow};
    use serde_json::json;

    struct Home {
        dir: tempfile::TempDir,
        paths: Paths,
        store: Store,
    }

    fn home() -> Home {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("data"), dir.path().join(".claude-rotate"))
            .with_home(dir.path());
        std::fs::create_dir_all(&paths.profile_dir).unwrap();
        Home {
            store: Store::open_in_memory().unwrap(),
            paths,
            dir,
        }
    }

    impl Home {
        fn account(&self, label: &str, uuid: &str) -> i64 {
            let ident = AccountIdentity {
                account_uuid: Some(uuid.into()),
                ..Default::default()
            };
            self.store
                .insert_account(label, &ident, AccountState::Parked)
                .unwrap()
        }

        /// A profile signed in to `signed_in`, with a cached 5-hour reading fetched at
        /// `fetched_at` and stamped with `stamped`.
        fn profile(&self, name: &str, signed_in: &str, stamped: &str, pct: f64, fetched_at: i64) {
            let dir = self.dir.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let config = json!({
                "oauthAccount": {"accountUuid": signed_in},
                "cachedUsageUtilization": {
                    "fetchedAtMs": fetched_at,
                    "accountUuid": stamped,
                    "utilization": {"limits": [
                        {"kind": "session", "percent": pct, "resets_at": "2026-10-25T03:40:00Z"}
                    ]}
                }
            });
            std::fs::write(dir.join(".claude.json"), config.to_string()).unwrap();
        }
    }

    const NOW: i64 = 1_792_900_000_000;

    #[test]
    fn records_what_another_profile_read_for_a_pool_account() {
        let h = home();
        let raid2 = h.account("raid2", "u2");
        h.profile(".claude-raid2", "u2", "u2", 100.0, NOW - 360_000);

        assert_eq!(import(&h.store, &h.paths, NOW).unwrap(), 1);
        let sample = h.store.last_sample(raid2).unwrap().unwrap();
        assert_eq!(
            sample.ts,
            NOW - 360_000,
            "kept at the time Claude Code read it"
        );
        assert_eq!(sample.profile, ".claude-raid2");
        assert_eq!(sample.windows[0].name, "five_hour");
        assert_eq!(sample.windows[0].pct, 100.0);

        assert_eq!(
            import(&h.store, &h.paths, NOW).unwrap(),
            0,
            "an unchanged config is not read again"
        );
    }

    #[test]
    fn ignores_readings_that_cannot_be_attributed_or_are_old() {
        let h = home();
        let a = h.account("a", "ua");
        h.account("b", "ub");
        // Taken before a /login switched the profile to another account.
        h.profile(".claude-switched", "ua", "ub", 100.0, NOW - 1000);
        // An account that is not in the pool.
        h.profile(".claude-other", "ux", "ux", 100.0, NOW - 1000);
        // The rotation profile's own cache is never trusted.
        h.profile(".claude-rotate", "ua", "ua", 100.0, NOW - 1000);
        // Stamped in the future.
        h.profile(".claude-future", "ub", "ub", 100.0, NOW + 3_600_000);
        assert_eq!(import(&h.store, &h.paths, NOW).unwrap(), 0);

        // Older than what the store already has.
        h.store
            .record_sample(&Sample {
                ts: NOW - 10_000,
                account_id: a,
                profile: "rotate".into(),
                session_id: None,
                model: None,
                windows: vec![SampleWindow {
                    name: "five_hour".into(),
                    pct: 20.0,
                    resets_at: None,
                }],
            })
            .unwrap();
        h.profile(".claude-a", "ua", "ua", 100.0, NOW - 60_000);
        assert_eq!(import(&h.store, &h.paths, NOW).unwrap(), 0);
        assert_eq!(
            h.store.last_sample(a).unwrap().unwrap().windows[0].pct,
            20.0
        );
    }

    #[test]
    fn the_default_profile_keeps_its_config_in_the_home_directory() {
        let h = home();
        let main = h.account("main", "um");
        std::fs::create_dir_all(h.dir.path().join(".claude")).unwrap();
        let config = json!({
            "oauthAccount": {"accountUuid": "um"},
            "cachedUsageUtilization": {
                "fetchedAtMs": NOW - 1000, "accountUuid": "um",
                "utilization": {"limits": [{"kind": "weekly_all", "percent": 41, "resets_at": null}]}
            }
        });
        std::fs::write(h.dir.path().join(".claude.json"), config.to_string()).unwrap();
        assert_eq!(import(&h.store, &h.paths, NOW).unwrap(), 1);
        assert_eq!(
            h.store.last_sample(main).unwrap().unwrap().windows[0].name,
            "seven_day"
        );
    }
}
