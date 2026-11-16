//! The pure rules the loopback relay follows: which requests it refuses, which get a pool login,
//! which headers pass, how usage windows are read from response headers, what counts as a limit
//! refusal and when to renew.
//!
//! The design is in `docs/specs/2026-11-10-relay-mode.md`, sections 5, 7 and 8.

use crate::relay::RelayConfig;
use crate::scheduler;
use crate::store::SampleWindow;

/// What the relay does with one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Not for the relay: answer with an error and forward nothing.
    Refuse,
    /// A model request: forwarded with a pool login. `path` has no `/r/<secret>` prefix, query kept.
    Model { path: String },
    /// Anything else the relay allows: forwarded with the caller's own headers.
    Pass { path: String },
    /// Exactly `GET /r/<secret>/__ccr/version`: the relay answers with its own version itself,
    /// asking Anthropic nothing.
    Version,
}

/// The path, after the secret, at which the relay answers with its own version (`doctor` asks).
pub const VERSION_PATH: &str = "/__ccr/version";

/// Compares two byte slices without stopping at the first difference.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Decides what to do with a request (spec 5, steps 1 and 2; the version request, spec 11).
pub fn route(
    method: &str,
    path_and_query: &str,
    host: Option<&str>,
    has_origin: bool,
    cfg: &RelayConfig,
) -> Route {
    // A short or empty secret would let any local process in, whatever relay.json held.
    if cfg.secret.len() < 32 || has_origin || !matches!(method, "GET" | "HEAD" | "POST") {
        return Route::Refuse;
    }
    if host != Some(format!("127.0.0.1:{}", cfg.port).as_str()) {
        return Route::Refuse;
    }
    let Some(after) = path_and_query.strip_prefix("/r/") else {
        return Route::Refuse;
    };
    let end = after.find(['/', '?']).unwrap_or(after.len());
    if !ct_eq(&after.as_bytes()[..end], cfg.secret.as_bytes()) {
        return Route::Refuse;
    }
    let rest = &after[end..];
    let path = if rest.is_empty() {
        "/".to_string()
    } else if rest.starts_with('?') {
        format!("/{rest}")
    } else {
        rest.to_string()
    };
    if method == "GET" && path == VERSION_PATH {
        return Route::Version;
    }
    let bare = path.split('?').next().unwrap_or("");
    if method == "POST" && matches!(bare, "/v1/messages" | "/v1/messages/count_tokens") {
        Route::Model { path }
    } else {
        Route::Pass { path }
    }
}

const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn hop_or_length(lower: &str) -> bool {
    lower == "content-length" || HOP_BY_HOP.contains(&lower)
}

/// Whether a request header goes on to Anthropic. A model request gets its own login, so the
/// caller's `authorization` stays behind.
pub fn forward_request_header(name: &str, model: bool) -> bool {
    let lower = name.to_ascii_lowercase();
    !(lower == "host" || hop_or_length(&lower) || (model && lower == "authorization"))
}

/// Whether a response header goes back to the caller.
pub fn forward_response_header(name: &str) -> bool {
    !hop_or_length(&name.to_ascii_lowercase())
}

/// Whether the session that sent a request with this `User-Agent` has a status line that counts
/// its cost at API prices (`statusline::note_cost`), so the relay must not count it again.
///
/// Claude Code names where it runs in its `User-Agent`: `claude-cli/<version> (external,
/// <entrypoint>, ...)`. Only an interactive terminal session (`cli`) runs a status line; a
/// session of the Claude desktop app (`claude-desktop`), `claude -p` (`sdk-cli`) and SDK clients
/// do not, and neither does a client that is not Claude Code.
pub fn status_line_counts_cost(user_agent: Option<&str>) -> bool {
    let Some(rest) = user_agent.and_then(|ua| ua.trim().strip_prefix("claude-cli/")) else {
        return false;
    };
    let Some(inside) = rest.split_once('(').and_then(|(_, r)| r.split_once(')')) else {
        return false;
    };
    inside.0.split(',').nth(1).map(str::trim) == Some("cli")
}

/// Claude Code's name for a window, from the name used in the rate-limit headers of a response
/// to a request for `model`.
///
/// `7d_oi` is the weekly limit of the requested model's family: it comes back on every Fable
/// request and on no other, at the percent and reset `/usage` shows as Fable's weekly limit.
/// So it is named the way `/usage` names that limit (`seven_day_fable`), and counts only for
/// that family. For a model cc-rotate does not know, it keeps the name it had.
pub fn window_name(header_window: &str, model: Option<&str>) -> String {
    match header_window {
        "5h" => "five_hour".into(),
        "7d" => "seven_day".into(),
        "7d_oi" => match model.and_then(scheduler::model_family) {
            Some(family) => format!("seven_day_{family}"),
            None => "seven_day_overage_included".into(),
        },
        "overage" => "overage".into(),
        other => other.into(),
    }
}

const UNIFIED: &str = "anthropic-ratelimit-unified-";

/// The usage windows in a response's headers, in the order their utilization headers appear,
/// for a request for `model` (`window_name`). A window needs a utilization that parses as a
/// finite number; `pct` is that value times 100.
pub fn readings<'a>(
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    model: Option<&str>,
) -> Vec<SampleWindow> {
    let all: Vec<(String, &str)> = headers
        .into_iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim()))
        .collect();
    let mut out: Vec<SampleWindow> = Vec::new();
    for (key, value) in &all {
        let Some(w) = key
            .strip_prefix(UNIFIED)
            .and_then(|k| k.strip_suffix("-utilization"))
        else {
            continue;
        };
        let Some(util) = value.parse::<f64>().ok().filter(|u| u.is_finite()) else {
            continue;
        };
        let name = window_name(w, model);
        if out.iter().any(|x| x.name == name) {
            continue;
        }
        let reset_key = format!("{UNIFIED}{w}-reset");
        let resets_at = all
            .iter()
            .find(|(k, _)| *k == reset_key)
            .and_then(|(_, v)| v.parse::<i64>().ok());
        out.push(SampleWindow {
            name,
            pct: util * 100.0,
            resets_at,
        });
    }
    out
}

/// A rate-limit refusal: the login is at a limit, as opposed to an overload or other error.
pub fn is_limit_refusal(status: u16, unified_status: Option<&str>) -> bool {
    status == 429 && unified_status == Some("rejected")
}

/// Whether a login that expires at `expires_at_ms` should be renewed now.
pub fn renew_due(expires_at_ms: i64, now_ms: i64, margin_s: i64) -> bool {
    expires_at_ms - now_ms <= margin_s * 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "0123456789abcdef0123456789abcdef";

    fn cfg() -> RelayConfig {
        RelayConfig {
            port: 4455,
            secret: S.into(),
        }
    }

    fn host() -> Option<&'static str> {
        Some("127.0.0.1:4455")
    }

    #[test]
    fn route_model() {
        let p = format!("/r/{S}/v1/messages?beta=true");
        assert_eq!(
            route("POST", &p, host(), false, &cfg()),
            Route::Model {
                path: "/v1/messages?beta=true".into()
            }
        );
        let p = format!("/r/{S}/v1/messages/count_tokens");
        assert_eq!(
            route("POST", &p, host(), false, &cfg()),
            Route::Model {
                path: "/v1/messages/count_tokens".into()
            }
        );
    }

    #[test]
    fn route_pass() {
        assert_eq!(
            route("HEAD", &format!("/r/{S}/api/hello"), host(), false, &cfg()),
            Route::Pass {
                path: "/api/hello".into()
            }
        );
        assert_eq!(
            route("GET", &format!("/r/{S}/v1/models"), host(), false, &cfg()),
            Route::Pass {
                path: "/v1/models".into()
            }
        );
        assert_eq!(
            route("GET", &format!("/r/{S}/v1/messages"), host(), false, &cfg()),
            Route::Pass {
                path: "/v1/messages".into()
            }
        );
    }

    #[test]
    fn route_refuse() {
        let c = cfg();
        let ok = format!("/r/{S}/v1/messages");
        assert_eq!(
            route("POST", "/r/wrong/v1/messages", host(), false, &c),
            Route::Refuse
        );
        assert_eq!(
            route("POST", "/v1/messages", host(), false, &c),
            Route::Refuse
        );
        assert_eq!(
            route("POST", &format!("/r/{S}x/v1/messages"), host(), false, &c),
            Route::Refuse
        );
        assert_eq!(
            route("POST", &ok, Some("localhost:4455"), false, &c),
            Route::Refuse
        );
        assert_eq!(route("POST", &ok, None, false, &c), Route::Refuse);
        assert_eq!(route("POST", &ok, host(), true, &c), Route::Refuse);
        assert_eq!(route("PUT", &ok, host(), false, &c), Route::Refuse);
    }

    #[test]
    fn route_version() {
        let c = cfg();
        let v = format!("/r/{S}/__ccr/version");
        assert_eq!(route("GET", &v, host(), false, &c), Route::Version);
        // Only exactly that request; the refusal rules come first.
        assert_eq!(
            route("HEAD", &v, host(), false, &c),
            Route::Pass {
                path: "/__ccr/version".into()
            }
        );
        assert_eq!(
            route("GET", &format!("{v}?x=1"), host(), false, &c),
            Route::Pass {
                path: "/__ccr/version?x=1".into()
            }
        );
        assert_eq!(
            route("GET", &format!("{v}/x"), host(), false, &c),
            Route::Pass {
                path: "/__ccr/version/x".into()
            }
        );
        assert_eq!(
            route("GET", "/r/wrong/__ccr/version", host(), false, &c),
            Route::Refuse
        );
        assert_eq!(
            route("GET", &v, Some("localhost:4455"), false, &c),
            Route::Refuse
        );
        assert_eq!(route("GET", &v, host(), true, &c), Route::Refuse);
    }

    #[test]
    fn route_refuse_empty_secret() {
        for secret in ["", "short"] {
            let c = RelayConfig {
                port: 4455,
                secret: secret.into(),
            };
            assert_eq!(
                route(
                    "POST",
                    &format!("/r/{secret}/v1/messages"),
                    host(),
                    false,
                    &c
                ),
                Route::Refuse
            );
        }
    }

    #[test]
    fn route_edge_cases() {
        let c = cfg();
        assert_eq!(
            route("POST", &format!("/r/{S}/../v1/messages"), host(), false, &c),
            Route::Pass {
                path: "/../v1/messages".into()
            }
        );
        assert_eq!(
            route("GET", &format!("/r/{S}?q=1"), host(), false, &c),
            Route::Pass {
                path: "/?q=1".into()
            }
        );
        assert_eq!(
            route("GET", &format!("/r/{S}"), host(), false, &c),
            Route::Pass { path: "/".into() }
        );
        assert_eq!(
            route("post", &format!("/r/{S}/v1/messages"), host(), false, &c),
            Route::Refuse
        );
        assert_eq!(
            route("HEAD", &format!("/r/{S}/v1/messages"), host(), false, &c),
            Route::Pass {
                path: "/v1/messages".into()
            }
        );
    }

    #[test]
    fn headers_rules() {
        assert!(!forward_request_header("Authorization", true));
        assert!(forward_request_header("authorization", false));
        assert!(forward_request_header("anthropic-beta", true));
        assert!(!forward_request_header("Host", false));
        assert!(!forward_request_header("Content-Length", false));
        assert!(!forward_request_header("Transfer-Encoding", false));
        assert!(!forward_response_header("Connection"));
        assert!(!forward_response_header("content-length"));
        assert!(forward_response_header("content-type"));
    }

    #[test]
    fn window_names() {
        assert_eq!(window_name("5h", None), "five_hour");
        assert_eq!(window_name("7d", None), "seven_day");
        assert_eq!(window_name("7d_oi", None), "seven_day_overage_included");
        assert_eq!(
            window_name("7d_oi", Some("claude-fable-5-2")),
            "seven_day_fable"
        );
        assert_eq!(
            window_name("7d_oi", Some("claude-opus-5-5")),
            "seven_day_opus"
        );
        assert_eq!(
            window_name("7d_oi", Some("gpt-x")),
            "seven_day_overage_included"
        );
        assert_eq!(window_name("overage", None), "overage");
        assert_eq!(window_name("7d_fable", None), "7d_fable");
    }

    #[test]
    fn readings_from_real_headers() {
        let h = [
            ("Anthropic-Ratelimit-Unified-5h-Utilization", "0.01"),
            ("anthropic-ratelimit-unified-5h-reset", "1794347400"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.0"),
            ("anthropic-ratelimit-unified-7d-reset", "1794499200"),
            ("content-type", "application/json"),
        ];
        let w = readings(h, None);
        assert_eq!(
            w,
            vec![
                SampleWindow {
                    name: "five_hour".into(),
                    pct: 1.0,
                    resets_at: Some(1794347400)
                },
                SampleWindow {
                    name: "seven_day".into(),
                    pct: 0.0,
                    resets_at: Some(1794499200)
                },
            ]
        );
        let w = readings(
            [
                ("anthropic-ratelimit-unified-7d_fable-utilization", "0.5"),
                ("anthropic-ratelimit-unified-5h-utilization", "abc"),
                ("anthropic-ratelimit-unified-7d-utilization", "NaN"),
            ],
            None,
        );
        assert_eq!(
            w,
            vec![SampleWindow {
                name: "7d_fable".into(),
                pct: 50.0,
                resets_at: None
            }]
        );
    }

    #[test]
    fn fable_weekly_from_a_fable_request() {
        let h = [
            ("anthropic-ratelimit-unified-7d-utilization", "0.44"),
            ("anthropic-ratelimit-unified-7d_oi-utilization", "0.8"),
            ("anthropic-ratelimit-unified-7d_oi-reset", "1794891600"),
        ];
        let w = readings(h, Some("claude-fable-5-2"));
        assert_eq!(w[1].name, "seven_day_fable");
        assert_eq!(w[1].pct, 80.0);
        assert_eq!(w[1].resets_at, Some(1794891600));
        let spec = scheduler::classify(&w[1].name);
        assert_eq!(spec.model.as_deref(), Some("fable"));
    }

    #[test]
    fn limit_refusal() {
        assert!(is_limit_refusal(429, Some("rejected")));
        assert!(!is_limit_refusal(429, Some("allowed")));
        assert!(!is_limit_refusal(429, None));
        assert!(!is_limit_refusal(529, Some("rejected")));
    }

    #[test]
    fn renew_timing() {
        let now = 1_000_000;
        assert!(!renew_due(now + 901_000, now, 900));
        assert!(renew_due(now + 900_000, now, 900));
    }

    #[test]
    fn only_an_interactive_session_counts_its_own_cost() {
        let counts = |ua| status_line_counts_cost(Some(ua));
        assert!(counts("claude-cli/2.1.322 (external, cli)"));
        assert!(counts("claude-cli/2.1.322 (external, cli, workload/cron)"));
        assert!(!counts("claude-cli/2.1.325 (external, claude-desktop)"));
        assert!(!counts("claude-cli/2.1.325 (external, sdk-cli)"));
        assert!(!counts(
            "claude-cli/2.1.325 (external, sdk-ts, agent-sdk/0.3.1)"
        ));
        assert!(!counts("claude-cli/2.1.325 (external, cli-like)"));
        assert!(!counts("curl/8.7.1"));
        assert!(!counts("claude-cli/2.1.325"));
        assert!(!status_line_counts_cost(None));
    }
}
