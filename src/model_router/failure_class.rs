//! Structured failure classification for LLM provider errors
//!
//! Replaces ad-hoc string matching with a typed `FailureClass` enum that
//! categorizes errors based on HTTP status codes and error message content.
//! Used by the auth profile manager, circuit breaker, and model router to
//! decide whether to retry, rotate keys, cooldown providers, or disable keys.

use serde::{Deserialize, Serialize};

use crate::error::SyscityError;

/// Classified failure type for a provider request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FailureClass {
    /// Temporary auth failure (401, expired token) — retry with key rotation
    AuthTemporary,
    /// Permanent auth failure (403, invalid key) — disable key
    AuthPermanent,
    /// Rate limit hit (429) — cooldown + retry with backoff
    RateLimit,
    /// Billing/quota exceeded — disable key/provider
    Billing,
    /// Cloud relay credit balance below the overdraft floor (403
    /// `insufficient_credits`) — fail fast, do NOT disable the session key
    /// (the credential stays valid; only the balance is exhausted)
    #[cfg(feature = "cloud")]
    InsufficientCredits,
    /// Service overloaded (502, 503) — retry with backoff
    Overloaded,
    /// Request timeout — retry
    Timeout,
    /// Connection-level error (reset, closed, broken pipe) — retry
    ConnectionError,
    /// Content policy violation (400 content filtered) — no retry
    ContentPolicy,
    /// Model not found (404) — suppress model
    ModelNotFound,
    /// Context length exceeded — no retry, need truncation
    ContextLength,
    /// Generic server error (500, 520) — retry with backoff
    ServerError,
    /// Unclassified error
    Unknown,
}

impl FailureClass {
    /// Classify an error from an optional HTTP status code and the error
    /// itself.
    ///
    /// When the status code is known (e.g. from an HTTP response), it is used
    /// as the primary signal.  Otherwise the error message is parsed.
    pub fn from_error(error: &SyscityError, status_code: Option<u16>) -> Self {
        if let Some(code) = status_code {
            return Self::from_status_code(code, error);
        }
        Self::from_error_string(error)
    }

    /// Whether this failure type is safe to retry.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::AuthTemporary
                | Self::RateLimit
                | Self::Overloaded
                | Self::Timeout
                | Self::ConnectionError
                | Self::ServerError
        )
    }

    /// Whether the current API key should be rotated to the next available key.
    pub fn should_rotate_key(&self) -> bool {
        matches!(self, Self::AuthTemporary | Self::RateLimit)
    }

    /// Whether the current API key should be permanently disabled.
    pub fn should_disable_key(&self) -> bool {
        matches!(self, Self::AuthPermanent | Self::Billing)
    }

    /// Whether the provider itself should be put on cooldown (circuit breaker).
    pub fn should_cooldown_provider(&self) -> bool {
        matches!(self, Self::RateLimit | Self::Overloaded | Self::ServerError | Self::Timeout)
    }

    /// Default backoff duration in seconds before retrying this failure type.
    pub fn default_backoff_secs(&self) -> u64 {
        match self {
            Self::RateLimit => 60,
            Self::Overloaded => 30,
            Self::ServerError => 15,
            Self::Timeout => 10,
            Self::AuthTemporary => 5,
            Self::ConnectionError => 5,
            _ => 0,
        }
    }

    /// Human-readable description of the failure class.
    pub fn description(&self) -> &'static str {
        match self {
            Self::AuthTemporary => "temporary authentication failure",
            Self::AuthPermanent => "permanent authentication failure",
            Self::RateLimit => "rate limit exceeded",
            Self::Billing => "billing or quota exceeded",
            #[cfg(feature = "cloud")]
            Self::InsufficientCredits => "insufficient credit balance",
            Self::Overloaded => "service overloaded",
            Self::Timeout => "request timeout",
            Self::ConnectionError => "connection error",
            Self::ContentPolicy => "content policy violation",
            Self::ModelNotFound => "model not found",
            Self::ContextLength => "context length exceeded",
            Self::ServerError => "server error",
            Self::Unknown => "unknown error",
        }
    }

    // ------------------------------------------------------------------
    // Private helpers
    // ------------------------------------------------------------------

    fn from_status_code(code: u16, error: &SyscityError) -> Self {
        match code {
            400 => {
                let msg = error.to_string().to_lowercase();
                if msg.contains("context")
                    || msg.contains("too long")
                    || msg.contains("max_tokens")
                    || msg.contains("token limit")
                {
                    Self::ContextLength
                } else if msg.contains("content")
                    || msg.contains("policy")
                    || msg.contains("safety")
                    || msg.contains("moderation")
                    || msg.contains("filtered")
                {
                    Self::ContentPolicy
                } else {
                    Self::Unknown
                }
            }
            401 => Self::AuthTemporary,
            403 => {
                let msg = error.to_string().to_lowercase();
                // Cloud relay credit gate (balance below the overdraft
                // floor) is checked before the generic billing keywords so
                // it is never classified as `Billing` (which would disable
                // the otherwise-valid session key).
                #[cfg(feature = "cloud")]
                if msg.contains("insufficient_credits") {
                    return Self::InsufficientCredits;
                }
                if msg.contains("billing")
                    || msg.contains("payment")
                    || msg.contains("quota")
                    || msg.contains("insufficient")
                {
                    Self::Billing
                } else {
                    Self::AuthPermanent
                }
            }
            404 => Self::ModelNotFound,
            408 => Self::Timeout,
            429 => Self::RateLimit,
            502 | 503 => Self::Overloaded,
            500 | 501 | 504 | 505 | 520..=599 => Self::ServerError,
            _ => Self::Unknown,
        }
    }

    fn from_error_string(error: &SyscityError) -> Self {
        let msg = error.to_string().to_lowercase();

        // Extract status code from message if present (e.g. "OpenAI API error 429:
        // ...")
        if let Some(code) = Self::extract_status_code(&msg) {
            return Self::from_status_code(code, error);
        }

        // Message-based classification for connection / transport errors
        if msg.contains("timeout") || msg.contains("timed out") {
            Self::Timeout
        } else if msg.contains("connection closed")
            || msg.contains("broken pipe")
            || msg.contains("connection reset")
            || msg.contains("reset by peer")
            || msg.contains("unexpected eof")
            || msg.contains("connection refused")
        {
            Self::ConnectionError
        } else if msg.contains("overloaded") || msg.contains("service unavailable") {
            Self::Overloaded
        } else if msg.contains("context length")
            || msg.contains("context window")
            || msg.contains("maximum context")
            || msg.contains("too many tokens")
            || msg.contains("max_tokens")
            || msg.contains("token limit")
        {
            // Providers often surface this without an HTTP status code (or the
            // code is lost when an error is rewrapped), so keyword-match it.
            Self::ContextLength
        } else {
            Self::Unknown
        }
    }

    /// Try to extract a 3-digit HTTP status code from the error message.
    fn extract_status_code(msg: &str) -> Option<u16> {
        // Look for patterns like "error 429:", "HTTP 401", "status 503"
        for prefix in ["error ", "http ", "status ", "api error "] {
            if let Some(pos) = msg.find(prefix) {
                let start = pos + prefix.len();
                let rest = &msg[start..];
                // Try to parse up to 3 digits
                let digits: String = rest
                    .chars()
                    .take(3)
                    .filter(|c| c.is_ascii_digit())
                    .collect();
                if digits.len() == 3 {
                    if let Ok(code) = digits.parse::<u16>() {
                        return Some(code);
                    }
                }
            }
        }

        // Also scan for a standalone 3-digit code anywhere in the message
        // (e.g. "429 rate limit", "401 unauthorized"). Only accept a digit run
        // of exactly 3 characters bounded by non-alphanumerics so numbers like
        // "250000 tokens" or "128k" do not yield bogus status codes.
        let chars: Vec<char> = msg.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i].is_ascii_digit() {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let run_len = i - start;
                let bounded_left = start == 0 || !chars[start - 1].is_alphanumeric();
                let bounded_right = i == chars.len() || !chars[i].is_alphanumeric();
                if run_len == 3 && bounded_left && bounded_right {
                    let digits: String = chars[start..i].iter().collect();
                    if let Ok(code) = digits.parse::<u16>() {
                        // Validate it's a known HTTP status code range
                        if (100..=599).contains(&code) {
                            return Some(code);
                        }
                    }
                }
            } else {
                i += 1;
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_status_code_401() {
        let err = SyscityError::ExternalService {
            source: "OpenAI API error 401: Unauthorized".into(),
            cause: None,
        };
        let class = FailureClass::from_error(&err, Some(401));
        assert_eq!(class, FailureClass::AuthTemporary);
        assert!(class.should_rotate_key());
        assert!(!class.should_disable_key());
        assert!(class.is_retryable());
    }

    #[test]
    fn test_from_status_code_403_billing() {
        let err = SyscityError::ExternalService {
            source: "OpenAI API error 403: Billing quota exceeded".into(),
            cause: None,
        };
        let class = FailureClass::from_error(&err, Some(403));
        assert_eq!(class, FailureClass::Billing);
        assert!(class.should_disable_key());
    }

    #[cfg(feature = "cloud")]
    #[test]
    fn test_403_insufficient_credits_classified_and_key_kept() {
        // Cloud relay credit gate: HTTP 403 with an `insufficient_credits`
        // body. Must classify as InsufficientCredits — the session key
        // stays valid (no disable, no rotation), the request is not
        // retried.
        let err = SyscityError::ExternalService {
            source: "HTTP 403: {\"error\":\"insufficient_credits\"}".into(),
            cause: None,
        };
        let class = FailureClass::from_error(&err, Some(403));
        assert_eq!(class, FailureClass::InsufficientCredits);
        assert!(!class.should_disable_key());
        assert!(!class.should_rotate_key());
        assert!(!class.is_retryable());
        assert!(!class.should_cooldown_provider());

        // Status code lost in rewrapping must still classify correctly.
        let class = FailureClass::from_error(&err, None);
        assert_eq!(class, FailureClass::InsufficientCredits);
    }

    #[test]
    fn test_from_status_code_429() {
        let err = SyscityError::ExternalService {
            source: "Anthropic API error 429: Rate limit".into(),
            cause: None,
        };
        let class = FailureClass::from_error(&err, Some(429));
        assert_eq!(class, FailureClass::RateLimit);
        assert_eq!(class.default_backoff_secs(), 60);
        assert!(class.should_cooldown_provider());
    }

    #[test]
    fn test_from_status_code_404_model_not_found() {
        let err = SyscityError::ExternalService {
            source: "Model gpt-99 not found".into(),
            cause: None,
        };
        let class = FailureClass::from_error(&err, Some(404));
        assert_eq!(class, FailureClass::ModelNotFound);
        assert!(!class.is_retryable());
    }

    #[test]
    fn test_from_status_code_400_context_length() {
        let err = SyscityError::ExternalService {
            source: "Context length too long".into(),
            cause: None,
        };
        let class = FailureClass::from_error(&err, Some(400));
        assert_eq!(class, FailureClass::ContextLength);
    }

    #[test]
    fn test_from_status_code_400_content_policy() {
        let err = SyscityError::ExternalService {
            source: "Content policy violation".into(),
            cause: None,
        };
        let class = FailureClass::from_error(&err, Some(400));
        assert_eq!(class, FailureClass::ContentPolicy);
    }

    #[test]
    fn test_from_status_code_503() {
        let err = SyscityError::ExternalService {
            source: "Service overloaded".into(),
            cause: None,
        };
        let class = FailureClass::from_error(&err, Some(503));
        assert_eq!(class, FailureClass::Overloaded);
        assert_eq!(class.default_backoff_secs(), 30);
    }

    #[test]
    fn test_from_error_string_timeout() {
        let err = SyscityError::Internal("request timed out".into());
        let class = FailureClass::from_error(&err, None);
        assert_eq!(class, FailureClass::Timeout);
    }

    #[test]
    fn test_extract_status_code_from_message() {
        let err = SyscityError::ExternalService {
            source: "OpenAI API error 429: Too many requests".into(),
            cause: None,
        };
        let class = FailureClass::from_error(&err, None);
        assert_eq!(class, FailureClass::RateLimit);
    }

    #[test]
    fn test_extract_status_code_from_message_anthropic() {
        let err = SyscityError::ExternalService {
            source: "Anthropic API error 401: invalid x-api-key".into(),
            cause: None,
        };
        let class = FailureClass::from_error(&err, None);
        assert_eq!(class, FailureClass::AuthTemporary);
    }

    #[test]
    fn test_from_error_string_context_length_without_status_code() {
        // Provider errors are often rewrapped (e.g. "Provider x failed: ...")
        // with the status code lost — keyword matching must still classify.
        let err = SyscityError::ExternalService {
            source: "Provider anthropic failed: prompt is too long: 250000 tokens > maximum \
                     context length"
                .into(),
            cause: None,
        };
        assert_eq!(FailureClass::from_error(&err, None), FailureClass::ContextLength);
    }

    #[test]
    fn test_from_error_string_context_length_variants() {
        for msg in [
            "context window exhausted",
            "maximum context is 128k tokens",
            "request exceeds max_tokens limit",
            "too many tokens in the request",
            "token limit exceeded",
        ] {
            let err = SyscityError::ExternalService {
                source: msg.into(),
                cause: None,
            };
            assert_eq!(
                FailureClass::from_error(&err, None),
                FailureClass::ContextLength,
                "msg: {}",
                msg
            );
        }
    }

    #[test]
    fn test_unknown_error() {
        let err = SyscityError::Internal("something weird".into());
        let class = FailureClass::from_error(&err, None);
        assert_eq!(class, FailureClass::Unknown);
        assert!(!class.is_retryable());
        assert!(!class.should_rotate_key());
    }

    #[test]
    fn test_backoff_values() {
        assert_eq!(FailureClass::RateLimit.default_backoff_secs(), 60);
        assert_eq!(FailureClass::Overloaded.default_backoff_secs(), 30);
        assert_eq!(FailureClass::ServerError.default_backoff_secs(), 15);
        assert_eq!(FailureClass::Timeout.default_backoff_secs(), 10);
        assert_eq!(FailureClass::AuthTemporary.default_backoff_secs(), 5);
        assert_eq!(FailureClass::ConnectionError.default_backoff_secs(), 5);
        assert_eq!(FailureClass::Unknown.default_backoff_secs(), 0);
    }

    #[test]
    fn test_descriptions() {
        assert_eq!(FailureClass::RateLimit.description(), "rate limit exceeded");
        assert_eq!(FailureClass::ContextLength.description(), "context length exceeded");
    }
}
