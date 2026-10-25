//! The plan an account is on, for people: "Max 5x", "Max 20x", "Team 5x", "Pro".
//!
//! Derived from the two fields Claude Code keeps in every login (`claudeAiOauth`):
//! `subscriptionType` (`max`, `team`, `pro`, `enterprise`, ...) and `rateLimitTier`
//! (`default_claude_max_5x`, `default_claude_max_20x`, ...). The tier's multiplier is shown for
//! any plan that has one, so a Team premium seat (a `team` login on `default_claude_max_5x`,
//! as Claude Code 2.1.310 treats it) reads "Team 5x". Both fields are refreshed on every stored
//! login (sign-in, renewal write-back and swap), so the label follows plan changes.

/// The label for a plan, or the raw subscription type when the plan is not one we know.
pub fn label(subscription_type: Option<&str>, rate_limit_tier: Option<&str>) -> Option<String> {
    let sub = subscription_type?.trim();
    if sub.is_empty() {
        return None;
    }
    let plan = match sub.to_ascii_lowercase().as_str() {
        "max" => "Max",
        "team" => "Team",
        "enterprise" => "Enterprise",
        "pro" => "Pro",
        "free" => "Free",
        _ => return Some(sub.to_string()),
    };
    let tier = rate_limit_tier.unwrap_or_default().to_ascii_lowercase();
    Some(match multiplier(&tier) {
        Some(x) => format!("{plan} {x}"),
        None => plan.to_string(),
    })
}

/// "5x" from `default_claude_max_5x`, "20x" from `default_claude_max_20x`.
fn multiplier(tier: &str) -> Option<&str> {
    let x = tier.rsplit('_').next()?;
    let digits = x.strip_suffix('x')?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        let l = |s: Option<&str>, t: Option<&str>| label(s, t);
        assert_eq!(
            l(Some("max"), Some("default_claude_max_5x")).as_deref(),
            Some("Max 5x")
        );
        assert_eq!(
            l(Some("max"), Some("default_claude_max_20x")).as_deref(),
            Some("Max 20x")
        );
        assert_eq!(l(Some("max"), None).as_deref(), Some("Max"));
        assert_eq!(
            l(Some("team"), Some("default_claude_max_5x")).as_deref(),
            Some("Team 5x")
        );
        assert_eq!(
            l(Some("team"), Some("default_claude_max_20x")).as_deref(),
            Some("Team 20x")
        );
        assert_eq!(
            l(Some("team"), Some("default_claude_team")).as_deref(),
            Some("Team")
        );
        assert_eq!(l(Some("team"), None).as_deref(), Some("Team"));
        assert_eq!(
            l(Some("pro"), Some("default_claude_pro")).as_deref(),
            Some("Pro")
        );
        assert_eq!(l(Some("enterprise"), None).as_deref(), Some("Enterprise"));
        assert_eq!(
            l(Some("galaxy_brain"), Some("x")).as_deref(),
            Some("galaxy_brain"),
            "unknown plans show the raw subscription type"
        );
        assert_eq!(l(None, Some("default_claude_max_5x")), None);
        assert_eq!(l(Some(""), None), None);
    }
}
