//! Remote desktop adapters.
//!
//! A benchmark harness owns the machine it evaluates on: it boots a VM,
//! restores a snapshot, exposes the desktop over HTTP, and scores the result
//! with its own evaluator. This module is the client for that arrangement, so
//! the rest of the computer stack — the session, the action ladder, element
//! ids, the evidence it attaches — runs unchanged against a machine that is
//! not this one.
//!
//! What an adapter here must **not** do is boot, snapshot or score. Those
//! belong to the harness, and its evaluator is the only honest source of a
//! benchmark number.
//!
//! ```text
//! ComputerSession → RemoteDesktopAdapter ── HTTP ──► VM desktop server
//! ```
//!
//! The first implementation speaks the OSWorld VM protocol
//! ([`osworld::OsworldDesktop`]); Cua Bench's container sessions are close
//! enough that the same adapter serves both through a thin bridge.

use std::time::Duration;

use crate::computer::Result;

pub mod osworld;

pub use osworld::{parse_at_spi, ActionSink, OsworldDesktop};

/// How to reach a remote desktop and how long to wait for it.
#[derive(Debug, Clone)]
pub struct RemoteDesktopConfig {
    /// Base URL of the desktop service, e.g. `http://10.0.2.15:5000`.
    pub base_url: String,
    /// Bearer token, for a service that asks for one. OSWorld's VM server does
    /// not; a self-hosted bridge might.
    pub auth_token: Option<String>,
    /// Per-request timeout.
    pub request_timeout: Duration,
    /// How long a captured screenshot is reused. Kept short and invalidated by
    /// every action: the ladder decides `SuspectedNoop` by comparing the
    /// frames before and after an action, and a stale "after" frame would
    /// make every successful click look like a no-op.
    pub screenshot_ttl: Duration,
}

impl RemoteDesktopConfig {
    /// A configuration for a desktop at `base_url`, with the defaults the
    /// benchmark adapters use.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            auth_token: None,
            request_timeout: Duration::from_secs(30),
            screenshot_ttl: Duration::from_millis(250),
        }
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }
}

/// Minimal HTTP transport shared by remote adapters.
///
/// It exists so an adapter can be tested against a fake server without
/// dragging in a second client, and so every failure is reported the same way:
/// which request, what status, and the first line of what came back.
pub(crate) struct Transport {
    client: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl Transport {
    pub(crate) fn new(config: &RemoteDesktopConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(|e| {
                crate::computer::ComputerError::ToolFailed(format!(
                    "could not build the remote desktop HTTP client: {e}"
                ))
            })?;
        Ok(Self {
            client,
            base: config.base_url.trim_end_matches('/').to_string(),
            token: config.auth_token.clone(),
        })
    }

    pub(crate) async fn get(&self, path: &str) -> Result<(Option<String>, Vec<u8>)> {
        self.send(reqwest::Method::GET, path, None).await
    }

    pub(crate) async fn post_json(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<(Option<String>, Vec<u8>)> {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(Option<String>, Vec<u8>)> {
        let url = format!("{}{}", self.base, path);
        let mut request = self.client.request(method.clone(), &url);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request.send().await.map_err(|e| {
            crate::computer::ComputerError::ToolFailed(format!(
                "remote desktop {method} {path} failed: {e}"
            ))
        })?;

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = response.bytes().await.map_err(|e| {
            crate::computer::ComputerError::ToolFailed(format!(
                "remote desktop {method} {path}: could not read the response: {e}"
            ))
        })?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes);
            let first_line = body
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(300)
                .collect::<String>();
            return Err(crate::computer::ComputerError::ToolFailed(format!(
                "remote desktop {method} {path} returned HTTP {status}: {first_line}"
            )));
        }

        Ok((content_type, bytes.to_vec()))
    }
}

/// Parse a JSON body, with a message that says which endpoint lied.
pub(crate) fn parse_json(path: &str, bytes: &[u8]) -> Result<serde_json::Value> {
    serde_json::from_slice(bytes).map_err(|e| {
        crate::computer::ComputerError::ToolFailed(format!(
            "remote desktop {path} did not return JSON: {e} (body: {})",
            String::from_utf8_lossy(bytes)
                .chars()
                .take(200)
                .collect::<String>()
        ))
    })
}

/// The `error` field of an OSWorld response envelope, when it holds one.
///
/// The server answers `{"output": ..., "error": null}`; an empty string is as
/// good as null, and some routes nest the message under `output`.
pub(crate) fn envelope_error(value: &serde_json::Value) -> Option<String> {
    match value.get("error")? {
        serde_json::Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
        serde_json::Value::Object(obj) => obj
            .get("message")
            .and_then(|m| m.as_str())
            .filter(|m| !m.trim().is_empty())
            .map(|m| m.trim().to_string()),
        _ => None,
    }
}

/// A string field from a response, accepting either a string or a
/// stringified-JSON wrapper around one.
pub(crate) fn string_field(value: &serde_json::Value, names: &[&str]) -> Option<String> {
    for name in names {
        let Some(field) = value.get(*name) else {
            continue;
        };
        match field {
            serde_json::Value::String(text) => return Some(text.clone()),
            serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                return Some(field.to_string())
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_error_is_not_an_error() {
        let value = serde_json::json!({ "output": "ok", "error": "" });
        assert!(envelope_error(&value).is_none());
        let value = serde_json::json!({ "output": "", "error": "no such file" });
        assert_eq!(envelope_error(&value).as_deref(), Some("no such file"));
        let value = serde_json::json!({ "error": { "message": "boom" } });
        assert_eq!(envelope_error(&value).as_deref(), Some("boom"));
    }

    #[test]
    fn string_fields_accept_wrapped_values() {
        let value = serde_json::json!({ "output": { "width": 1920, "height": 1080 } });
        assert_eq!(
            string_field(&value, &["width", "output"]).as_deref(),
            Some("{\"height\":1080,\"width\":1920}")
        );
        let value = serde_json::json!({ "AT": "<desktop-frame/>" });
        assert_eq!(string_field(&value, &["AT", "output"]).as_deref(), Some("<desktop-frame/>"));
        assert!(string_field(&value, &["missing"]).is_none());
    }

    #[test]
    fn the_transport_trims_a_trailing_slash() {
        let mut config = RemoteDesktopConfig::new("http://127.0.0.1:5000/");
        config.base_url = config.base_url.trim_end_matches('/').to_string();
        assert_eq!(config.base_url, "http://127.0.0.1:5000");
        assert!(Transport::new(&config).is_ok());
    }
}
