//! The plan an account is on, for people: "Max 5x", "Team Premium", "Pro".
//!
//! Derived from the two fields Claude Code keeps in every login (`claudeAiOauth`):
//! `subscriptionType` (`max`, `team`, `pro`, `enterprise`, ...) and `rateLimitTier`
//! (`default_claude_max_5x`, `default_claude_max_20x`, ...). Claude Code 2.1.310 itself treats a
//! `team` login with the `default_claude_max_5x` tier as a premium Team seat. Both fields are
//! refreshed on every stored login (sign-in, renewal write-back and swap), so the label follows
//! plan changes.

/// The label for a plan, or the raw subscription type when the plan is not one we know.
pub fn label(subscription_type: Option<&str>, rate_limit_tier: Option<&str>) -> Option<String> {
    let sub = subscription_type?.trim();
    if sub.is_empty() {
        return None;
    }
    let tier = rate_limit_tier.unwrap_or_default().to_ascii_lowercase();
    let text = match sub.to_ascii_lowercase().as_str() {
        "max" if tier.contains("max_20x") => "Max 20x",
        "max" if tier.contains("max_5x") => "Max 5x",
        "max" => "Max",
        "team" if tier == "default_claude_max_5x" => "Team Premium",
        "team" => "Team",
        "enterprise" => "Enterprise",
        "pro" => "Pro",
        "free" => "Free",
        _ => return Some(sub.to_string()),
    };
    Some(text.to_string())
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
            Some("Team Premium")
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
