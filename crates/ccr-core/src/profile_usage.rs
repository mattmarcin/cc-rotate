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
use serde::Deserialize;
use serde_json::Value;

use crate::paths::Paths;
use crate::store::{Sample, SampleWindow, Store};
use crate::{credfile, profiles, usage_report};

/// Prefix of the meta keys that remember each config file's modification time when it was
/// last read.
const META_SEEN_PREFIX: &str = "profile_usage_seen:";

/// The two parts of a `.claude.json` this reads; the rest of the file is skipped unparsed.
#[derive(Deserialize)]
struct ConfigView {
    #[serde(rename = "oauthAccount")]
    oauth_account: Option<Value>,
    #[serde(rename = "cachedUsageUtilization")]
    cached: Option<Value>,
}

/// What a profile's config says about a pool account.
enum Found {
    /// Nothing usable: no identity, no cached reading, or one that cannot be attributed.
    Nothing,
    /// Signed in to this account, which is not in the pool (it may be added later).
    NotInPool(String),
    /// A reading for a pool account, recorded if it is newer than the account's latest.
    Reading(Sample),
}

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
        if swaps_logins(&dir) {
            continue;
        }
        let config = profiles::config_file_for(&dir, &paths.home);
        let Some(mtime) = credfile::mtime_ms(&config) else {
            continue;
        };
        let key = format!("{META_SEEN_PREFIX}{}", config.display());
        if unchanged(store, &key, mtime)? {
            continue;
        }
        let Ok(bytes) = std::fs::read(&config) else {
            continue;
        };
        let Ok(view) = serde_json::from_slice::<ConfigView>(&bytes) else {
            continue;
        };
        let seen = match reading(store, &dir, &view, now_ms)? {
            // Read again once the account joins the pool, even if the file never changes.
            Found::NotInPool(uuid) => format!("{mtime}:{uuid}"),
            Found::Reading(sample) => {
                if store.record_sample(&sample)? {
                    recorded += 1;
                }
                // Stamped by Claude Code with the account: its own reset times.
                crate::swap::note_known_resets(
                    store,
                    sample.account_id,
                    &sample.windows,
                    now_ms / 1000,
                )?;
                mtime.to_string()
            }
            Found::Nothing => mtime.to_string(),
        };
        store.set_meta(&key, seen)?;
    }
    Ok(recorded)
}

/// Whether a config was read at this modification time already, and nothing that would change
/// the outcome happened since: `<mtime>`, or `<mtime>:<account uuid>` for a profile signed in
/// to an account that was not in the pool then (and still is not).
fn unchanged(store: &Store, key: &str, mtime: i64) -> Result<bool> {
    let Some(seen) = store.meta(key)? else {
        return Ok(false);
    };
    let (at, outside) = match seen.split_once(':') {
        Some((at, uuid)) => (at, Some(uuid)),
        None => (seen.as_str(), None),
    };
    if at.parse::<i64>().ok() != Some(mtime) {
        return Ok(false);
    }
    Ok(match outside {
        Some(uuid) => store.account_by_uuid(uuid)?.is_none(),
        None => true,
    })
}

/// A profile whose status line is cc-rotate's own (not observe mode) is a rotation profile, or a
/// copy of one: its login changes account, so its cached reading cannot be attributed (F14).
fn swaps_logins(dir: &Path) -> bool {
    let Ok(Some(settings)) = credfile::read_document(&dir.join("settings.json")) else {
        return false;
    };
    let command = settings
        .get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    command.contains("cc-rotate-ctl")
        && command.contains("statusline")
        && !command.contains("--observe")
}

/// The reading a profile's config holds for a pool account, if it is usable and new.
fn reading(store: &Store, dir: &Path, view: &ConfigView, now_ms: i64) -> Result<Found> {
    let text = |key: &str| {
        view.oauth_account
            .as_ref()
            .and_then(|a| a.get(key))
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
    };
    let (Some(uuid), Some(report)) = (
        text("accountUuid"),
        view.cached.as_ref().and_then(usage_report::from_cache),
    ) else {
        return Ok(Found::Nothing);
    };
    // Claude Code stamps the reading with the account the profile was signed in to when it
    // fetched it; one taken before a `/login` to another account must not count for this one.
    if report.account_uuid.as_deref() != Some(uuid) || report.windows.is_empty() {
        return Ok(Found::Nothing);
    }
    let Some(account) = store.account_by_uuid(uuid)? else {
        return Ok(Found::NotInPool(uuid.to_string()));
    };
    // Limits belong to an organization, and one person can be a member of several with the
    // same account id (a personal plan and a Team seat, say).
    if account.org_uuid.is_none() || text("organizationUuid") != account.org_uuid.as_deref() {
        return Ok(Found::Nothing);
    }
    // Written on this machine, so a time ahead of now is clock trouble; never let it hide the
    // live readings that follow.
    let ts = report.fetched_at_ms.min(now_ms);
    let last = store.last_sample(account.id)?;
    if last.as_ref().is_some_and(|l| l.ts >= ts) {
        return Ok(Found::Nothing);
    }
    Ok(Found::Reading(Sample {
        ts,
        account_id: account.id,
        // Named like the readings observe mode records for the profile.
        profile: dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        session_id: None,
        model: None,
        windows: with_carried(report.windows, last.as_ref(), now_ms / 1000),
    }))
}

/// A usage report only has the windows it can express (`usage_report::expresses`). When it
/// becomes the latest reading, the previous reading's other windows (from the status line)
/// would stop counting; keep the ones whose reset has not passed.
pub fn with_carried(
    mut windows: Vec<SampleWindow>,
    last: Option<&Sample>,
    now_s: i64,
) -> Vec<SampleWindow> {
    for w in last.iter().flat_map(|l| &l.windows) {
        let live = w.resets_at.is_some_and(|r| r > now_s);
        if live && !usage_report::expresses(&w.name) && !windows.iter().any(|n| n.name == w.name) {
            windows.push(w.clone());
        }
    }
    windows.sort_by(|a, b| a.name.cmp(&b.name));
    windows
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
                org_uuid: Some(ORG.into()),
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
                "oauthAccount": {"accountUuid": signed_in, "organizationUuid": ORG},
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
    const ORG: &str = "org-1";

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
    fn a_reading_from_another_organization_of_the_same_person_does_not_count() {
        let h = home();
        let seat = h.account("seat", "u1");
        let dir = h.dir.path().join(".claude");
        std::fs::create_dir_all(&dir).unwrap();
        let config = json!({
            "oauthAccount": {"accountUuid": "u1", "organizationUuid": "personal-org"},
            "cachedUsageUtilization": {
                "fetchedAtMs": NOW - 1000, "accountUuid": "u1",
                "utilization": {"limits": [{"kind": "session", "percent": 100, "resets_at": null}]}
            }
        });
        std::fs::write(dir.join(".claude.json"), config.to_string()).unwrap();
        assert_eq!(import(&h.store, &h.paths, NOW).unwrap(), 0);
        assert!(h.store.last_sample(seat).unwrap().is_none());
    }

    #[test]
    fn an_account_added_later_gets_the_reading_its_profile_already_had() {
        let h = home();
        h.profile(".claude-b", "ub", "ub", 100.0, NOW - 60_000);
        assert_eq!(import(&h.store, &h.paths, NOW).unwrap(), 0);
        let b = h.account("b", "ub");
        assert_eq!(import(&h.store, &h.paths, NOW + 5000).unwrap(), 1);
        assert_eq!(
            h.store.last_sample(b).unwrap().unwrap().windows[0].pct,
            100.0
        );
    }

    #[test]
    fn keeps_the_status_line_windows_a_report_cannot_express() {
        let h = home();
        let a = h.account("a", "ua");
        let now_s = NOW / 1000;
        h.store
            .record_sample(&Sample {
                ts: NOW - 600_000,
                account_id: a,
                profile: "rotate".into(),
                session_id: None,
                model: None,
                windows: vec![
                    SampleWindow {
                        name: "five_hour".into(),
                        pct: 10.0,
                        resets_at: Some(now_s + 3600),
                    },
                    SampleWindow {
                        name: "spend_limit".into(),
                        pct: 99.0,
                        resets_at: Some(now_s + 86_400),
                    },
                    SampleWindow {
                        name: "overage".into(),
                        pct: 50.0,
                        resets_at: Some(now_s - 10),
                    },
                ],
            })
            .unwrap();
        h.profile(".claude-a", "ua", "ua", 30.0, NOW - 1000);
        assert_eq!(import(&h.store, &h.paths, NOW).unwrap(), 1);
        let last = h.store.last_sample(a).unwrap().unwrap();
        let names: Vec<_> = last.windows.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["five_hour", "spend_limit"],
            "the reset one is gone"
        );
        assert_eq!(
            last.windows[0].pct, 30.0,
            "the report replaces what it covers"
        );
    }

    #[test]
    fn a_copy_of_a_rotation_profile_is_not_trusted() {
        let h = home();
        let a = h.account("a", "ua");
        h.profile(".claude-rotate-old", "ua", "ua", 100.0, NOW - 1000);
        let settings =
            |cmd: &str| json!({"statusLine": {"type": "command", "command": cmd}}).to_string();
        let dir = h.dir.path().join(".claude-rotate-old");
        std::fs::write(
            dir.join("settings.json"),
            settings("cc-rotate-ctl statusline"),
        )
        .unwrap();
        assert_eq!(import(&h.store, &h.paths, NOW).unwrap(), 0);

        // Observe mode is a profile with its own login, which is fine.
        std::fs::write(
            dir.join("settings.json"),
            settings("cc-rotate-ctl statusline --observe"),
        )
        .unwrap();
        assert_eq!(import(&h.store, &h.paths, NOW).unwrap(), 1);
        assert!(h.store.last_sample(a).unwrap().is_some());
    }

    #[test]
    fn a_reading_from_the_future_is_kept_at_now() {
        let h = home();
        let a = h.account("a", "ua");
        h.profile(".claude-a", "ua", "ua", 50.0, NOW + 3_600_000);
        assert_eq!(import(&h.store, &h.paths, NOW).unwrap(), 1);
        assert_eq!(h.store.last_sample(a).unwrap().unwrap().ts, NOW);
    }

    #[cfg(unix)]
    #[test]
    fn follows_a_linked_profile_directory() {
        let h = home();
        let a = h.account("a", "ua");
        let real = h.dir.path().join("dotfiles-claude-a");
        std::fs::create_dir_all(&real).unwrap();
        h.profile("dotfiles-claude-a", "ua", "ua", 70.0, NOW - 1000);
        std::os::unix::fs::symlink(&real, h.dir.path().join(".claude-a")).unwrap();
        assert_eq!(import(&h.store, &h.paths, NOW).unwrap(), 1);
        assert_eq!(
            h.store.last_sample(a).unwrap().unwrap().windows[0].pct,
            70.0
        );
    }

    #[test]
    fn the_default_profile_keeps_its_config_in_the_home_directory() {
        let h = home();
        let main = h.account("main", "um");
        std::fs::create_dir_all(h.dir.path().join(".claude")).unwrap();
        let config = json!({
            "oauthAccount": {"accountUuid": "um", "organizationUuid": ORG},
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
