//! Error types for Syscity
//!
//! This module defines all error types used throughout the application.
//! It uses `thiserror` for defining structured errors that can be
//! easily converted to user-facing messages.

use std::path::PathBuf;

use thiserror::Error;

/// The main error type for Syscity operations
#[derive(Error, Debug)]
pub enum SyscityError {
    /// Configuration-related errors
    #[error("Configuration error: {0}")]
    Config(#[from] ConfigError),

    /// I/O errors
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// I/O errors with additional context
    #[error("I/O error: {context}: {source}")]
    IoContext {
        context: String,
        #[source]
        source: std::io::Error,
    },

    /// HTTP client errors
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// Serialization errors
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Validation errors
    #[error("Validation error: {0}")]
    Validation(String),

    /// Resource not found
    #[error("Resource not found: {resource}")]
    NotFound { resource: String },

    /// Storage errors (database, file system, etc.)
    #[error("Storage error: {context} - {details}")]
    Storage { context: String, details: String },

    /// Internal errors (should not be exposed to users)
    #[error("Internal error: {0}")]
    Internal(String),

    /// External service errors
    #[error("External service error: {source}")]
    ExternalService {
        source: String,
        #[source]
        cause: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// Plugin errors (WASM extension errors)
    #[error("Plugin error: {0}")]
    Plugin(String),

    /// Subagent exceeded maximum spawn depth
    #[error("Maximum subagent spawn depth ({0}) exceeded")]
    MaxSpawnDepth(u32),

    /// Too many concurrent subagents
    #[error("Maximum concurrent subagents ({0}) already active")]
    MaxConcurrentSubagents(usize),

    /// Subagent timed out waiting for completion
    #[error("Subagent timed out waiting for completion")]
    SubagentTimeout,

    /// Tool or operation timed out
    #[error("Timed out: {0}")]
    Timeout(String),

    /// Subagent run not found
    #[error("Subagent run not found")]
    SubagentNotFound,

    /// Subagent execution failed
    #[error("Subagent failed: {0}")]
    SubagentFailed(String),

    /// Subagent was killed
    #[error("Subagent was killed")]
    SubagentKilled,

    /// Sandbox policy violation
    #[error("Sandbox violation: {0}")]
    SandboxViolation(String),

    /// Operation not supported by this channel/provider
    #[error("Unsupported: {0}")]
    Unsupported(String),
}

/// Configuration-specific errors
#[derive(Error, Debug)]
pub enum ConfigError {
    /// Failed to read config file
    #[error("Failed to read config file at '{path}': {source}")]
    FileRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Failed to parse config file
    #[error("Failed to parse config file: {0}")]
    Parse(String),

    /// Missing required configuration
    #[error("Missing required configuration: {0}")]
    Missing(String),

    /// Invalid configuration value
    #[error("Invalid configuration value for '{key}': {message}")]
    InvalidValue { key: String, message: String },

    /// Environment variable error
    #[error("Environment variable error: {0}")]
    Env(#[from] std::env::VarError),
}

/// Result type alias for Syscity operations
pub type Result<T> = std::result::Result<T, SyscityError>;

/// Extension trait for adding context to results
pub trait ResultExt<T, E> {
    /// Add context to an error
    fn with_context<F, C>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> C,
        C: Into<String>;
}

impl<T> ResultExt<T, std::io::Error> for std::result::Result<T, std::io::Error> {
    fn with_context<F, C>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> C,
        C: Into<String>,
    {
        self.map_err(|e| SyscityError::IoContext { context: f().into(), source: e })
    }
}

impl From<toml::ser::Error> for SyscityError {
    fn from(err: toml::ser::Error) -> Self {
        SyscityError::Internal(format!("TOML serialization error: {}", err))
    }
}

impl From<toml::de::Error> for SyscityError {
    fn from(err: toml::de::Error) -> Self {
        SyscityError::Internal(format!("TOML deserialization error: {}", err))
    }
}

impl From<serde_norway::Error> for SyscityError {
    fn from(err: serde_norway::Error) -> Self {
        SyscityError::Internal(format!("YAML error: {}", err))
    }
}

impl From<crate::computer::ComputerError> for SyscityError {
    fn from(err: crate::computer::ComputerError) -> Self {
        SyscityError::Internal(format!("Computer adapter error: {}", err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = SyscityError::Validation("test error".to_string());
        assert_eq!(err.to_string(), "Validation error: test error");
    }

    #[test]
    fn test_config_error_display() {
        let err = ConfigError::Missing("api_key".to_string());
        assert_eq!(err.to_string(), "Missing required configuration: api_key");
    }

    #[test]
    fn test_error_variants_display() {
        let err =
            SyscityError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "file missing"));
        assert!(err.to_string().contains("I/O error"));

        let err = SyscityError::Internal("something broke".to_string());
        assert!(err.to_string().contains("Internal error: something broke"));

        let err = SyscityError::NotFound { resource: "user".to_string() };
        assert!(err.to_string().contains("Resource not found: user"));

        let err = SyscityError::Storage {
            context: "db".to_string(),
            details: "connection failed".to_string(),
        };
        assert!(err
            .to_string()
            .contains("Storage error: db - connection failed"));

        let err = SyscityError::Plugin("wasm error".to_string());
        assert!(err.to_string().contains("Plugin error: wasm error"));

        let err = SyscityError::MaxSpawnDepth(5);
        assert!(err
            .to_string()
            .contains("Maximum subagent spawn depth (5) exceeded"));

        let err = SyscityError::MaxConcurrentSubagents(10);
        assert!(err
            .to_string()
            .contains("Maximum concurrent subagents (10) already active"));

        let err = SyscityError::SubagentTimeout;
        assert!(err
            .to_string()
            .contains("Subagent timed out waiting for completion"));

        let err = SyscityError::SubagentNotFound;
        assert!(err.to_string().contains("Subagent run not found"));

        let err = SyscityError::SubagentFailed("crash".to_string());
        assert!(err.to_string().contains("Subagent failed: crash"));

        let err = SyscityError::SubagentKilled;
        assert!(err.to_string().contains("Subagent was killed"));

        let err = SyscityError::SandboxViolation("no network".to_string());
        assert!(err.to_string().contains("Sandbox violation: no network"));
    }

    #[test]
    fn test_config_error_variants() {
        let err = ConfigError::Parse("bad toml".to_string());
        assert!(err
            .to_string()
            .contains("Failed to parse config file: bad toml"));

        let err = ConfigError::InvalidValue {
            key: "port".to_string(),
            message: "not a number".to_string(),
        };
        assert!(err
            .to_string()
            .contains("Invalid configuration value for 'port': not a number"));

        let err = ConfigError::FileRead {
            path: PathBuf::from("/tmp/config.toml"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        };
        assert!(err.to_string().contains("Failed to read config file"));
    }

    #[test]
    fn test_external_service_error_display() {
        let err = SyscityError::ExternalService {
            source: "openai".to_string(),
            cause: Some(Box::new(std::io::Error::new(std::io::ErrorKind::Other, "timeout"))),
        };
        assert!(err.to_string().contains("External service error: openai"));
    }

    #[test]
    fn test_result_ext_io() {
        let result: std::result::Result<i32, std::io::Error> =
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"));
        let syscity_result: Result<i32> = result.with_context(|| "file op");
        assert!(syscity_result.is_err());
        assert!(syscity_result.unwrap_err().to_string().contains("file op"));
    }
}
