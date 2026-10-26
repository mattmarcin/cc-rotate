//! Claude Code's plan usage report: what `/usage` shows. Claude Code fetches it from
//! `GET /api/oauth/usage` with the profile's login and caches the response body in the
//! profile's `.claude.json` as `cachedUsageUtilization` (`{fetchedAtMs, accountUuid,
//! utilization}`), stamped with the account it was fetched for (spec F16).
//!
//! The body has one field per limit system (`five_hour`, `seven_day`, `seven_day_opus`, and many
//! codenamed ones that are usually null) and, in current versions, `limits[]`: the rows `/usage`
//! displays, each with a `kind` (`session`, `weekly_all`, `weekly_scoped`), a `percent`, a
//! `resets_at` and an optional model or surface scope. Rows are turned into windows named the
//! way the status line names them, so both sources describe a limit identically:
//!
//! | row | window |
//! |---|---|
//! | `session` | `five_hour` |
//! | `session` scoped to a model family | `five_hour_<family>` |
//! | `weekly_all` | `seven_day` |
//! | `weekly_scoped` scoped to a model family | `seven_day_<family>` |
//!
//! Rows of another kind, rows scoped to a surface other than Claude Code's own, and model scopes
//! of a family cc-rotate does not know are left out. Unlike the status line, which only reports
//! what limits Claude Code, the report covers every product on the plan, so guessing would park
//! an account for days over a limit that does not apply; a limit that does apply and was missed
//! here shows up in the status line on the account's first request, and the account is then
//! left at once (`scheduler::Timing::leaves_at_once`).

use serde_json::{Map, Value};

use crate::scheduler::{self, FIVE_HOUR, SEVEN_DAY};
use crate::store::{ExtraUsage, SampleWindow};
use crate::time;

/// The key Claude Code caches the report under in `.claude.json`.
pub const CACHE_KEY: &str = "cachedUsageUtilization";

/// Top-level fields read when a report has no `limits[]` (older responses).
const LEGACY_FIELDS: &[&str] = &[FIVE_HOUR, SEVEN_DAY, "seven_day_opus", "seven_day_sonnet"];

/// A usage report as Claude Code cached it.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedReport {
    /// When Claude Code fetched it (epoch ms).
    pub fetched_at_ms: i64,
    /// The account Claude Code fetched it for, when it recorded one.
    pub account_uuid: Option<String>,
    pub windows: Vec<SampleWindow>,
    /// The account's extra usage, when the plan has it (`extra_usage`).
    pub extra_usage: Option<ExtraUsage>,
}

/// The report cached in a `.claude.json` document, if it has one.
pub fn from_config(doc: &Map<String, Value>) -> Option<CachedReport> {
    from_cache(doc.get(CACHE_KEY)?)
}

/// A `cachedUsageUtilization` value.
pub fn from_cache(cached: &Value) -> Option<CachedReport> {
    let cached = cached.as_object()?;
    Some(CachedReport {
        fetched_at_ms: time::epoch_ms(cached.get("fetchedAtMs")?.as_f64()?)?,
        account_uuid: cached
            .get("accountUuid")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .map(str::to_string),
        windows: windows(cached.get("utilization")?),
        extra_usage: cached.get("utilization").and_then(extra_usage),
    })
}

/// The `extra_usage` of a report body: amounts in minor units of its currency (cents for USD),
/// `is_enabled` false while extra usage cannot cover requests.
pub fn extra_usage(body: &Value) -> Option<ExtraUsage> {
    let e = body.get("extra_usage")?.as_object()?;
    let minor = |k: &str| e.get(k).and_then(Value::as_f64).map(|v| v.round() as i64);
    let text = |k: &str| {
        e.get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    Some(ExtraUsage {
        enabled: e.get("is_enabled")?.as_bool()?,
        monthly_limit: minor("monthly_limit"),
        used: minor("used_credits"),
        utilization: e.get("utilization").and_then(Value::as_f64),
        currency: text("currency").map(|c| c.to_ascii_uppercase()),
        disabled_reason: text("disabled_reason"),
    })
}

/// Whether a report can hold a window of this name: the ones `windows` produces. Any other
/// window (from the status line) is simply not something a report describes.
pub fn expresses(name: &str) -> bool {
    let family = |rest: &str| scheduler::model_family(rest) == Some(rest);
    match name {
        FIVE_HOUR | SEVEN_DAY => true,
        _ => name
            .strip_prefix("five_hour_")
            .or_else(|| name.strip_prefix("seven_day_"))
            .is_some_and(family),
    }
}

/// Every usage window in a report body, named as the status line names them.
pub fn windows(report: &Value) -> Vec<SampleWindow> {
    let mut out: Vec<SampleWindow> = Vec::new();
    match report.get("limits").and_then(Value::as_array) {
        Some(rows) if !rows.is_empty() => {
            for row in rows {
                if let Some(w) = row_window(row) {
                    merge(&mut out, w);
                }
            }
        }
        _ => {
            for field in LEGACY_FIELDS {
                let Some(limit) = report.get(*field).filter(|v| v.is_object()) else {
                    continue;
                };
                if let Some(pct) = limit.get("utilization").and_then(Value::as_f64) {
                    merge(
                        &mut out,
                        SampleWindow {
                            name: field.to_string(),
                            pct,
                            resets_at: resets_at(limit.get("resets_at")),
                        },
                    );
                }
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn row_window(row: &Value) -> Option<SampleWindow> {
    let scope = row.get("scope").filter(|s| s.is_object());
    let named = |key: &str| {
        scope
            .and_then(|s| s.get(key))
            .filter(|v| v.is_object())
            .and_then(|v| v.get("display_name"))
            .and_then(Value::as_str)
    };
    if let Some(surface) = named("surface") {
        if !surface.eq_ignore_ascii_case("claude code") {
            return None;
        }
    }
    let family = match named("model") {
        Some(model) => Some(scheduler::model_family(model)?),
        None => None,
    };
    let name = match (row.get("kind")?.as_str()?, family) {
        ("session", None) => FIVE_HOUR.to_string(),
        ("session", Some(f)) => format!("{FIVE_HOUR}_{f}"),
        ("weekly_all", None) => SEVEN_DAY.to_string(),
        ("weekly_scoped", Some(f)) => format!("{SEVEN_DAY}_{f}"),
        _ => return None,
    };
    Some(SampleWindow {
        name,
        pct: row.get("percent")?.as_f64()?,
        resets_at: resets_at(row.get("resets_at")),
    })
}

/// Two rows that map to one window keep the higher reading.
fn merge(out: &mut Vec<SampleWindow>, w: SampleWindow) {
    match out.iter_mut().find(|o| o.name == w.name) {
        Some(o) if w.pct > o.pct => *o = w,
        Some(_) => {}
        None => out.push(w),
    }
}

/// An RFC 3339 timestamp, or epoch seconds.
fn resets_at(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::String(s) => time::parse_rfc3339(s),
        Value::Number(n) => n.as_f64().and_then(time::epoch_s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(w: &[SampleWindow]) -> Vec<&str> {
        w.iter().map(|w| w.name.as_str()).collect()
    }

    /// A Team seat as Claude Code 2.1.310 cached it: a session row and a Fable weekly row, no
    /// weekly limit on all models, and many codenamed fields that are null or not limits.
    #[test]
    fn team_seat() {
        let report = json!({
            "five_hour": {"utilization": 100.0, "resets_at": "2026-10-25T03:40:00.147485+00:00"},
            "seven_day": null,
            "seven_day_opus": null,
            "tangelo": null,
            "extra_usage": {"is_enabled": true, "utilization": 99.0, "monthly_limit": 10},
            "spend": {"percent": 50, "used": {"amount_minor": 0}},
            "limits": [
                {"kind": "session", "group": "session", "percent": 100, "severity": "critical",
                 "resets_at": "2026-10-25T03:40:00.147485+00:00", "scope": null, "is_active": true},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 15, "severity": "normal",
                 "resets_at": "2026-10-28T13:00:00+00:00",
                 "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null},
                 "is_active": false}
            ]
        });
        let w = windows(&report);
        assert_eq!(names(&w), vec!["five_hour", "seven_day_fable"]);
        let extra = extra_usage(&report).unwrap();
        assert!(extra.enabled);
        assert_eq!(extra.monthly_limit, Some(10));
        assert_eq!(extra.used, None);
        assert_eq!(w[0].pct, 100.0);
        assert_eq!(w[0].resets_at, time::parse_rfc3339("2026-10-25T03:40:00Z"));
        assert_eq!(w[1].pct, 15.0);
    }

    /// Extra usage as Claude Code 2.1.310 cached it, turned off by the user and in use.
    #[test]
    fn extra_usage_states() {
        let off = json!({"extra_usage": {"is_enabled": false, "monthly_limit": null,
            "used_credits": null, "utilization": null, "currency": null, "decimal_places": null,
            "disabled_reason": null, "user_disabled": true, "spend_limit_reached": false}});
        let off = extra_usage(&off).unwrap();
        assert!(!off.enabled);
        assert_eq!(
            (off.monthly_limit, off.used, off.currency),
            (None, None, None)
        );

        let on = json!({"extra_usage": {"is_enabled": true, "monthly_limit": 5000,
            "used_credits": 1240.0, "utilization": 24.8, "currency": "usd"}});
        let on = extra_usage(&on).unwrap();
        assert_eq!((on.monthly_limit, on.used), (Some(5000), Some(1240)));
        assert_eq!(on.currency.as_deref(), Some("USD"));

        assert!(extra_usage(&json!({"extra_usage": null})).is_none());
        assert!(extra_usage(&json!({})).is_none());
    }

    #[test]
    fn max_account_with_a_weekly_limit() {
        let report = json!({"limits": [
            {"kind": "session", "percent": 9, "resets_at": "2026-10-24T07:59:00Z"},
            {"kind": "weekly_all", "percent": 28.4, "resets_at": "2026-10-30T09:59:00Z"},
            {"kind": "weekly_scoped", "percent": 40, "resets_at": "2026-10-30T09:59:00Z",
             "scope": {"model": {"display_name": "Opus 4.5"}}},
            {"kind": "weekly_scoped", "percent": 55, "resets_at": "2026-10-30T09:59:00Z",
             "scope": {"model": {"display_name": "Opus"}}}
        ]});
        let w = windows(&report);
        assert_eq!(names(&w), vec!["five_hour", "seven_day", "seven_day_opus"]);
        assert_eq!(
            w[2].pct, 55.0,
            "two rows for one family keep the higher reading"
        );
    }

    #[test]
    fn leaves_out_what_may_not_limit_claude_code() {
        let report = json!({"limits": [
            {"kind": "session", "percent": 5, "resets_at": null},
            {"kind": "weekly_scoped", "percent": 100, "scope": {"model": {"display_name": "Mystery"}}},
            {"kind": "weekly_all", "percent": 100, "scope": {"surface": {"display_name": "Cowork"}}},
            {"kind": "daily_spend", "percent": 100},
            {"kind": "weekly_all", "percent": 30, "scope": {"surface": {"display_name": "Claude Code"}}},
            {"kind": "session"}
        ]});
        let w = windows(&report);
        assert_eq!(names(&w), vec!["five_hour", "seven_day"]);
        assert_eq!(w[0].resets_at, None);
        assert_eq!(w[1].pct, 30.0);
    }

    #[test]
    fn what_a_report_can_express() {
        for name in [
            "five_hour",
            "seven_day",
            "seven_day_opus",
            "five_hour_fable",
        ] {
            assert!(expresses(name), "{name}");
        }
        for name in [
            "spend_limit",
            "seven_day_oauth_apps",
            "seven_day_mystery",
            "overage",
        ] {
            assert!(!expresses(name), "{name}");
        }
    }

    #[test]
    fn older_reports_without_rows() {
        let report = json!({
            "five_hour": {"utilization": 41.0, "resets_at": "2026-10-24T18:00:00Z"},
            "seven_day": {"utilization": 12.0, "resets_at": "2026-10-30T00:00:00Z"},
            "seven_day_sonnet": {"utilization": null, "resets_at": null},
            "cinder_cove": {"utilization": 100.0},
            "limits": []
        });
        assert_eq!(names(&windows(&report)), vec!["five_hour", "seven_day"]);
        assert!(windows(&json!({})).is_empty());
        assert!(windows(&json!("garbage")).is_empty());
    }

    #[test]
    fn reads_the_cache_in_a_config() {
        let doc = json!({
            "numStartups": 4,
            "cachedUsageUtilization": {
                "fetchedAtMs": 1_792_900_000_000_i64,
                "accountUuid": "u2",
                "utilization": {"limits": [{"kind": "session", "percent": 100, "resets_at": "2026-10-25T03:40:00Z"}]}
            }
        });
        let report = from_config(doc.as_object().unwrap()).unwrap();
        assert_eq!(report.fetched_at_ms, 1_792_900_000_000);
        assert_eq!(report.account_uuid.as_deref(), Some("u2"));
        assert_eq!(names(&report.windows), vec!["five_hour"]);

        assert!(from_config(json!({"numStartups": 1}).as_object().unwrap()).is_none());
        let unstamped = json!({"cachedUsageUtilization": {"fetchedAtMs": 1, "utilization": {}}});
        assert_eq!(
            from_config(unstamped.as_object().unwrap())
                .unwrap()
                .account_uuid,
            None
        );
    }
}
