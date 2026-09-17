//! TUI-specific error types.

use std::fmt;

use thiserror::Error;

/// Errors that can occur inside the TUI client.
#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum TuiError {
    /// Terminal I/O failure.
    #[error("terminal error: {0}")]
    Terminal(#[from] std::io::Error),

    /// WebSocket connection or protocol failure.
    #[error("websocket error: {0}")]
    WebSocket(String),

    /// The gateway did not answer in time.
    #[error("timed out waiting for `{0}`")]
    Timeout(String),

    /// Gateway returned an error response.
    #[error("gateway error {code}: {message}")]
    Gateway { code: String, message: String },

    /// Authentication failure.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// A requested operation is not allowed (e.g. missing scope).
    #[error("not allowed: {0}")]
    NotAllowed(String),

    /// Invalid user input.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Serialization failure.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// A backend that cannot fail — ratatui's `TestBackend` is one — has no error
/// to convert. Without this, `event_loop::run` could only be driven by a real
/// terminal backend, and the loop would stay untested.
impl From<std::convert::Infallible> for TuiError {
    fn from(never: std::convert::Infallible) -> Self {
        match never {}
    }
}

impl TuiError {
    /// Whether this ends the session rather than the command that hit it.
    ///
    /// The distinction the UI needs: a command that fails is a line of output
    /// and the user carries on; a gateway that is unreachable retries; but a
    /// terminal that cannot be drawn to leaves nothing to carry on with.
    /// Killing the TUI over a timed-out `/status` was the old behaviour, and
    /// it is why this split exists.
    pub fn is_fatal(&self) -> bool {
        match self {
            Self::Terminal(_) | Self::Serialization(_) => true,
            Self::WebSocket(_)
            | Self::Timeout(_)
            | Self::Gateway { .. }
            | Self::Auth(_)
            | Self::NotAllowed(_)
            | Self::InvalidInput(_) => false,
        }
    }

    /// Build a gateway error from code and message.
    #[allow(dead_code)]
    pub fn gateway(code: impl fmt::Display, message: impl fmt::Display) -> Self {
        Self::Gateway {
            code: code.to_string(),
            message: message.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the failures the UI cannot recover from end the session.
    #[test]
    fn only_a_broken_terminal_or_a_bug_is_fatal() {
        assert!(TuiError::Terminal(std::io::Error::other("tty gone")).is_fatal());
        assert!(TuiError::Serialization(
            serde_json::from_str::<serde_json::Value>("{").expect_err("invalid json")
        )
        .is_fatal());

        // Everything a command can fail with leaves the TUI running.
        assert!(!TuiError::Timeout("system.presence".to_string()).is_fatal());
        assert!(!TuiError::WebSocket("lost".to_string()).is_fatal());
        assert!(!TuiError::gateway("NOT_FOUND", "gone").is_fatal());
        assert!(!TuiError::Auth("no token".to_string()).is_fatal());
        assert!(!TuiError::NotAllowed("needs admin".to_string()).is_fatal());
        assert!(!TuiError::InvalidInput("bad".to_string()).is_fatal());
    }
}
