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
    /// Build a gateway error from code and message.
    #[allow(dead_code)]
    pub fn gateway(code: impl fmt::Display, message: impl fmt::Display) -> Self {
        Self::Gateway {
            code: code.to_string(),
            message: message.to_string(),
        }
    }
}
