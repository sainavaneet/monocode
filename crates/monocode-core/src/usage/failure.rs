//! Classifying provider errors for recovery.
use super::Limit;

/// Why a provider turn failed, as recovery sees it. Each category maps to one
/// action (back off, cool the account down, switch, or block); see
/// `tasks::recovery`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    /// Network errors, 5xx, overload, short rate limits: retry the same account.
    Temporary,
    /// The account's own usage or quota is spent ([`Limit::from_error`]).
    UsageExhausted,
    /// An organization or workspace limit shared by several accounts.
    SharedQuota,
    /// Expired, invalid or revoked credentials.
    Auth,
    /// The provider binary is missing or crashed at start.
    ProviderUnavailable,
    /// Unknown model, invalid parameters, unsupported options.
    Configuration,
    /// Content or permission refusals.
    Policy,
    Unknown,
}

fn matches(pattern: &'static std::sync::OnceLock<regex::Regex>, source: &str, text: &str) -> bool {
    pattern
        .get_or_init(|| regex::Regex::new(source).unwrap())
        .is_match(text)
}

impl Failure {
    pub const ALL: [Self; 8] = [
        Self::Temporary,
        Self::UsageExhausted,
        Self::SharedQuota,
        Self::Auth,
        Self::ProviderUnavailable,
        Self::Configuration,
        Self::Policy,
        Self::Unknown,
    ];

    /// Classifies a provider's error text. Order matters: a spent limit
    /// (shared first, then the account's own) wins over everything, because
    /// providers report it under other codes (Anthropic: a low credit balance
    /// is an `invalid_request_error`); then configuration, an OS-level spawn
    /// failure, policy, credentials, and only then retryable errors. Status
    /// numbers count only at the start or after "status/http/code/error".
    pub fn classify(message: &str) -> Self {
        static CONFIGURATION: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        static POLICY: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        static AUTH: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        static SHARED: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        static UNAVAILABLE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        static TEMPORARY: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let text = message.trim();
        if matches(
            &SHARED,
            r"(?i)\b(?:organi[sz]ation|org|workspace|team|enterprise)(?:'s)?\b[^.\n]{0,40}\b(?:limit|quota|credits?)\b[^.\n]{0,30}\b(?:reached|exceeded|exhausted|depleted)",
            text,
        ) {
            return Self::SharedQuota;
        }
        if Limit::from_error(text).is_some() {
            return Self::UsageExhausted;
        }
        if matches(
            &CONFIGURATION,
            r"(?i)model_not_found|\bmodel\b[^\n]{0,80}\b(?:does not exist|not found|is not (?:available|supported))|unknown model|invalid[_ ](?:request[_ ]error|parameter|argument)|unsupported[_ ](?:option|parameter|value|model)|unrecognized (?:option|argument)",
            text,
        ) {
            return Self::Configuration;
        }
        if matches(
            &UNAVAILABLE,
            r"(?i)not installed|command not found|no such file or directory|\benoent\b|\(os error \d+\)|failed to (?:spawn|start|launch)|could not (?:start|launch|find) (?:the )?(?:provider|codex|claude|cli)|(?:exited|crashed) (?:unexpectedly|with (?:code|status|signal))",
            text,
        ) {
            return Self::ProviderUnavailable;
        }
        if matches(
            &POLICY,
            r"(?i)content[_ ](?:policy|filter)|policy[_ ]violation|violates? (?:our |the )?(?:usage |content )?polic|permission[_ ]denied|not permitted|\bforbidden\b|(?:^|\b(?:status|http|code|error)\b[^0-9\n]{0,12})403\b",
            text,
        ) {
            return Self::Policy;
        }
        if matches(
            &AUTH,
            r"(?i)(?:^|\b(?:status|http|code|error)\b[^0-9\n]{0,12})401\b|unauthori[sz]ed|invalid[_ ](?:api[_ ]key|token|credentials?|grant)|(?:token|credentials?|session|login) (?:has |have )?(?:expired|been revoked|revoked)|authentication[_ ](?:required|failed|error)|not (?:signed|logged) in|not authenticated|please (?:log ?in|sign in)|no longer signed in|re-?authenticate",
            text,
        ) {
            return Self::Auth;
        }
        if matches(
            &TEMPORARY,
            r"(?i)(?:^|\b(?:status|http|code|error)\b[^0-9\n]{0,12})(?:429|500|502|503|504|529)\b|rate[_ ]?limit|too many requests|overloaded|timed? ?out|timeout|temporar(?:y|ily)|try again|connection (?:reset|refused|closed|error)|network|econnreset|etimedout|service unavailable|bad gateway|internal server error|stream (?:error|disconnected)",
            text,
        ) {
            return Self::Temporary;
        }
        Self::Unknown
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Temporary => "temporary",
            Self::UsageExhausted => "usage_exhausted",
            Self::SharedQuota => "shared_quota",
            Self::Auth => "auth",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::Configuration => "configuration",
            Self::Policy => "policy",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == text)
    }

    /// Whether recovery may move the work to another account of the same
    /// provider. Configuration and policy failures would fail the same way
    /// anywhere, a shared quota is spent for every account sharing it, and an
    /// unknown failure is not evidence the account is at fault.
    pub fn may_switch_accounts(self) -> bool {
        matches!(self, Self::UsageExhausted | Self::Auth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_classified_into_recovery_categories() {
        use Failure::*;
        for (message, expected) in [
            ("503 Service Unavailable", Temporary),
            ("Overloaded: please try again", Temporary),
            ("429: rate limit exceeded, retry in 3 seconds", Temporary),
            ("Network timeout", Temporary),
            ("stream disconnected before completion", Temporary),
            ("usage_limit_reached", UsageExhausted),
            ("rate_limit", UsageExhausted),
            (
                "You've hit your session limit · resets 5:30pm",
                UsageExhausted,
            ),
            ("insufficient_quota", UsageExhausted),
            (
                "Your organization's usage limit has been reached",
                SharedQuota,
            ),
            ("Workspace credits exhausted for this team", SharedQuota),
            ("401 Unauthorized", Auth),
            ("Your access token has expired. Please log in again.", Auth),
            ("invalid_api_key", Auth),
            ("Not signed in", Auth),
            ("codex is not installed", ProviderUnavailable),
            (
                "failed to spawn provider: No such file or directory",
                ProviderUnavailable,
            ),
            ("model_not_found: gpt-9 does not exist", Configuration),
            (
                "The model `o9` does not exist or you do not have access",
                Configuration,
            ),
            (
                "invalid_request_error: unsupported parameter",
                Configuration,
            ),
            ("This request violates our usage policy", Policy),
            ("content_policy_violation", Policy),
            ("permission denied by policy", Policy),
            ("Something odd happened", Unknown),
            // Anthropic reports a spent balance as an invalid request.
            (
                "invalid_request_error: Your credit balance is too low to access the API",
                UsageExhausted,
            ),
            // An OS spawn error is the provider, not a policy refusal.
            ("Permission denied (os error 13)", ProviderUnavailable),
            ("403 Forbidden", Policy),
            ("HTTP status 403", Policy),
            ("status code: 500", Temporary),
            ("Wrote 500 lines", Unknown),
            ("Read 403 files", Unknown),
            ("The build took 503ms", Unknown),
            ("", Unknown),
        ] {
            assert_eq!(Failure::classify(message), expected, "{message:?}");
        }
        for failure in [
            Temporary,
            UsageExhausted,
            SharedQuota,
            Auth,
            ProviderUnavailable,
            Configuration,
            Policy,
            Unknown,
        ] {
            assert_eq!(Failure::parse(failure.as_str()), Some(failure));
        }
        // Only configuration and policy failures never move to another account.
        assert!(!Configuration.may_switch_accounts());
        assert!(!Policy.may_switch_accounts());
        assert!(!SharedQuota.may_switch_accounts());
        assert!(!Unknown.may_switch_accounts());
        assert!(UsageExhausted.may_switch_accounts() && Auth.may_switch_accounts());
    }
}
