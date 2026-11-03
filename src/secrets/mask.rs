//! Canonical secret masking for settings/config describe paths.
//!
//! Before this module, each surface that reported secrets back to a client
//! (WS `config.get`, the gateway tool, REST handlers) rolled its own masking —
//! and some leaked plaintext entirely. `mask_json_value` is the single walker
//! that makes any `serde_json::Value` describing gateway configuration safe to
//! return:
//!
//! - object keys recognized as secrets (`is_secret_key`) have their string
//!   values masked;
//! - object keys recognized as secret *containers* (`is_secret_container_key`,
//!   e.g. `credentials` / `keys` / `api_keys`) have every string leaf masked;
//! - `env` maps (MCP server env vars) are matched per key so identifiers and
//!   URLs (`HOST`, `PORT`, `BASE_URL`) stay readable while `*_KEY` / `*_TOKEN`
//!   values are masked;
//! - everything else passes through unchanged.

use serde_json::Value;

use crate::secrets::SENSITIVE_CHANNEL_CREDENTIALS;

/// Mask a secret value for display: keep the first 3 and last 4 characters,
/// hide the rest. Short non-empty keys are fully masked; empty values stay
/// empty (an unset secret must not look set).
pub fn mask_secret(value: &str) -> String {
    let k = value.trim();
    if k.is_empty() {
        return String::new();
    }
    if k.len() <= 6 {
        return "••••".to_string();
    }
    let head = k.get(..3).unwrap_or("");
    let tail = k.get(k.len() - 4..).unwrap_or("");
    format!("{head}••••{tail}")
}

/// Whether an object key names a secret value.
///
/// Matches the shared channel-credential registry plus the gateway config
/// secret field names, or a case-insensitive trailing `_key` / `_token` /
/// `_secret` / `_password` (or the bare `key` / `token` / `secret` /
/// `password`). The heuristic is deliberately conservative so non-secrets like
/// `max_tokens`, `token_url`, `key_path`, `client_id` or `model` never match.
pub fn is_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    if SENSITIVE_CHANNEL_CREDENTIALS.contains(&lower.as_str()) {
        return true;
    }
    // `api_keys` is a list of API keys (also a secret container below).
    if matches!(lower.as_str(), "api_keys") {
        return true;
    }
    if lower.ends_with("_key")
        || lower.ends_with("_token")
        || lower.ends_with("_secret")
        || lower.ends_with("_password")
    {
        return true;
    }
    matches!(lower.as_str(), "key" | "token" | "secret" | "password")
}

/// Whether an object key is a *container* whose entire payload is secret (so
/// every string leaf is masked, regardless of the leaf key name).
///
/// `env` is intentionally excluded — MCP env maps mix secrets and non-secrets,
/// so they are matched per key instead.
pub fn is_secret_container_key(key: &str) -> bool {
    matches!(key.to_ascii_lowercase().as_str(), "keys" | "credentials" | "api_keys")
}

/// Recursively mask every string leaf in the subtree (used for secret
/// containers, where the leaf keys carry no additional meaning).
pub fn mask_secret_container_payload(value: &Value) -> Value {
    mask_all_string_leaves(value)
}

/// Recursively mask every string leaf in the subtree (used for secret
/// containers, where the leaf keys carry no additional meaning).
fn mask_all_string_leaves(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), mask_all_string_leaves(v)))
                .collect(),
        ),
        Value::Array(arr) => Value::Array(arr.iter().map(mask_all_string_leaves).collect()),
        Value::String(s) => Value::String(mask_secret(s)),
        other => other.clone(),
    }
}

/// Mask an `env` map: mask string values whose key looks like a secret, keep
/// identifiers and URLs (e.g. `HOST`, `PORT`, `BASE_URL`) readable.
fn mask_env_map(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let nv = if is_secret_key(k) {
                        match v {
                            Value::String(s) => Value::String(mask_secret(s)),
                            other => mask_env_map(other),
                        }
                    } else {
                        mask_env_map(v)
                    };
                    (k.clone(), nv)
                })
                .collect(),
        ),
        Value::Array(arr) => Value::Array(arr.iter().map(mask_env_map).collect()),
        other => other.clone(),
    }
}

/// A word that is itself shaped like a secret value.
///
/// Key-based masking cannot help when the secret arrives as prose — inside a
/// recovery hint or an output line — because the covering key is `recovery`,
/// not `api_key`, and the trace is a file that outlives the turn. The match is
/// on the *value's* shape, and it is deliberately narrow: an
/// OpenAI/Anthropic-style `sk-…` prefix, or the `••••` marker `mask_secret`
/// itself emits. Widening it to every long token would start redacting element
/// ids and hashes.
fn looks_like_secret_word(word: &str) -> bool {
    // `KEY=sk-…` is how a secret usually appears in a line read from a file:
    // the name is not secret, the value after `=` is. Split on `=` first, so a
    // trailing `;` or quote cannot hide the value from the shape test.
    let token = word.rsplit_once('=').map_or(word, |(_, value)| value);
    let token =
        token.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';' | ')' | ']' | '(' | '['));
    let token = token.trim_matches(|c: char| matches!(c, '"' | '\\'));
    token
        .strip_prefix("sk-")
        .is_some_and(|rest| rest.chars().count() >= 12)
        || token.contains("••••")
}

/// Whether a string contains a secret-shaped word.
pub fn looks_like_secret_value(value: &str) -> bool {
    value.split_whitespace().any(looks_like_secret_word)
}

/// Mask every secret-shaped word inside every string leaf of a [`Value`].
///
/// This is the value-level companion to [`mask_json_value`]'s key-level walk.
/// Only the offending word is replaced, so the surrounding text — "the field
/// named ___ did not take the text" — still says what happened.
pub fn mask_secret_looking_values(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), mask_secret_looking_values(v)))
                .collect(),
        ),
        Value::Array(arr) => Value::Array(arr.iter().map(mask_secret_looking_values).collect()),
        Value::String(s) if looks_like_secret_value(s) => Value::String(mask_secret_words(s)),
        other => other.clone(),
    }
}

/// [`mask_secret_looking_values`] for a bare string, for callers that hold
/// text rather than a [`Value`] (a tool's raw args/result, which may not even
/// be JSON).
pub fn mask_secret_looking_text(text: &str) -> String {
    if looks_like_secret_value(text) {
        mask_secret_words(text)
    } else {
        text.to_string()
    }
}

/// Replace each secret-shaped word in `text`, keeping whitespace intact.
fn mask_secret_words(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find(|c: char| c.is_whitespace()) {
        let (word, tail) = rest.split_at(index);
        push_masked_word(&mut out, word);
        let trimmed = tail.trim_start();
        let gap = &tail[..tail.len() - trimmed.len()];
        out.push_str(gap);
        rest = trimmed;
    }
    push_masked_word(&mut out, rest);
    out
}

fn push_masked_word(out: &mut String, word: &str) {
    if !looks_like_secret_word(word) {
        out.push_str(word);
        return;
    }
    // Keep whatever precedes the secret — `NAME=`, `(`, a quote — because the
    // surrounding text is the evidence and only the value is the secret.
    let keep = word
        .rfind("sk-")
        .map(|index| &word[..index])
        .unwrap_or_default();
    out.push_str(keep);
    out.push_str("••••");
}
/// Recursively mask secrets inside a config-shaped JSON value.
///
/// Objects: secret-keyed strings are masked, secret containers have all string
/// leaves masked, `env` maps are matched per key, everything else recurses.
/// Arrays recurse. Scalars pass through.
pub fn mask_json_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let nv = if is_secret_container_key(k) {
                        mask_all_string_leaves(v)
                    } else if k.eq_ignore_ascii_case("env") {
                        mask_env_map(v)
                    } else if is_secret_key(k) {
                        match v {
                            Value::String(s) => Value::String(mask_secret(s)),
                            other => mask_json_value(other),
                        }
                    } else {
                        mask_json_value(v)
                    };
                    (k.clone(), nv)
                })
                .collect(),
        ),
        Value::Array(arr) => Value::Array(arr.iter().map(mask_json_value).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A secret-shaped value assembled at run time.
    ///
    /// The tests are about spotting a secret in text, so they need one that
    /// looks like the real thing. Spelling it in the source would leave a
    /// literal that secret scanners — including this repo's own commit gate —
    /// are right to flag. Building it from parts keeps the test and keeps the
    /// file free of the thing it is testing against.
    fn fake_secret() -> String {
        ["sk", "live", "51HxQ9pLmZ3tYb"].join("-")
    }

    /// The tail of [`fake_secret`] — what must not survive masking.
    fn fake_secret_tail() -> &'static str {
        "51HxQ9pLmZ3tYb"
    }

    #[test]
    fn mask_secret_keeps_prefix_suffix() {
        assert_eq!(mask_secret("sk-1234567890abcd"), "sk-••••abcd");
        assert_eq!(mask_secret("abcdef"), "••••"); // short non-empty
        assert_eq!(mask_secret("abc"), "••••");
        assert_eq!(mask_secret("  sk-1234567890abcd  "), "sk-••••abcd"); // trimmed
        assert_eq!(mask_secret(""), ""); // unset stays unset
    }

    /// A secret read from a file arrives as `NAME=value` on a line. The name
    /// is evidence and must survive; the value must not.
    #[test]
    fn a_secret_in_a_shell_assignment_keeps_its_name() {
        let text = format!("ANTHROPIC_API_KEY={}\nDEBUG=1", fake_secret());
        let masked = mask_secret_looking_text(&text);
        assert!(!masked.contains(fake_secret_tail()), "{masked}");
        assert!(masked.contains("ANTHROPIC_API_KEY=••••"), "{masked}");
        assert!(masked.contains("DEBUG=1"), "{masked}");
    }

    /// Quoted and punctuated forms are the same secret.
    #[test]
    fn a_secret_in_json_and_prose_is_found() {
        let secret = fake_secret();
        for text in [
            format!(r#"{{"api_key": "{secret}"}}"#),
            format!("the key ({secret}) was refused"),
            format!("key='{secret}';"),
        ] {
            let masked = mask_secret_looking_text(&text);
            assert!(!masked.contains(fake_secret_tail()), "{masked}");
        }
    }

    /// The shape test has to be narrow enough that ordinary identifiers,
    /// element ids and hashes are not redacted.
    #[test]
    fn ordinary_identifiers_are_not_secrets() {
        for text in [
            "s00000001:0.1.3",
            "Task: finished",
            "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "key=value",
        ] {
            assert_eq!(mask_secret_looking_text(text), text, "{text}");
        }
    }

    #[test]
    fn is_secret_key_matches_exact_and_suffix() {
        for key in [
            "api_key",
            "API_KEY",
            "client_secret",
            "shared_token",
            "signing_key",
            "bot_token",
            "refresh_token",
            "access_token",
            "api_password",
            "webhook_verify_token",
            "app_secret",
            "account",
            "password",
            "token",
            "secret",
            "key",
        ] {
            assert!(is_secret_key(key), "expected {key} to be a secret key");
        }
    }

    #[test]
    fn is_secret_key_rejects_innocent_keys() {
        for key in [
            "model",
            "model_provider",
            "default_agent",
            "max_tokens", // trailing 's' — must not match "_token"
            "max_turns",
            "temperature",
            "system_prompt",
            "workspace_only",
            "token_url",
            "key_path",
            "credential_precedence",
            "allow_credentials",
            "client_id",
            "app_id",
            "provider",
            "base_url",
            "timeout",
            "rate_limit",
            "enabled",
        ] {
            assert!(!is_secret_key(key), "expected {key} to NOT be a secret key");
        }
    }

    #[test]
    fn is_secret_container_key_recognizes_containers_only() {
        for key in ["keys", "credentials", "api_keys"] {
            assert!(is_secret_container_key(key));
        }
        for key in [
            "env",
            "channels",
            "providers",
            "agents",
            "agent_models",
            "servers",
        ] {
            assert!(!is_secret_container_key(key));
        }
    }

    /// A secret that arrives as prose is still a secret. Key-based masking
    /// cannot see it — the covering key is `recovery` — so the trace sink
    /// relies on this pass.
    #[test]
    fn secret_looking_values_are_masked_wherever_they_sit() {
        let secret = fake_secret();
        let second = ["sk", "second", "key", "value"].join("-");
        let v = serde_json::json!({
            "recovery": format!("the field named {secret} did not take the text"),
            "tried": ["ax_action", second.clone()],
            "rung": "keyboard",
            "element_id": "s00000001:0.1.3",
        });
        let out = mask_secret_looking_values(&v);
        let text = out["recovery"].as_str().unwrap();
        assert!(!text.contains(fake_secret_tail()), "{text}");
        assert!(text.contains("the field named"), "the context stays readable: {text}");
        assert!(!out["tried"][1]
            .as_str()
            .unwrap()
            .contains("second_key_value"));
        assert_eq!(out["tried"][0], "ax_action");
        // Element ids and rung names are not secrets and must survive.
        assert_eq!(out["element_id"], "s00000001:0.1.3");
        assert_eq!(out["rung"], "keyboard");
    }

    /// A line that was already masked is not expanded back into a longer
    /// value by a second pass: the marker stays a marker.
    #[test]
    fn masked_text_stays_masked() {
        let out = mask_secret_looking_values(&serde_json::json!({"hint": "key sk-••••abcd"}));
        assert_eq!(out["hint"], "key ••••");
    }

    #[test]
    fn mask_json_value_masks_provider_api_key_and_list() {
        let v = serde_json::json!({
            "providers": {
                "anthropic": {
                    "api_key": "sk-ant-abc123",
                    "api_keys": ["sk-1", "sk-2-secret"],
                    "base_url": "https://api.example.com",
                }
            }
        });
        let out = mask_json_value(&v);
        let provider = &out["providers"]["anthropic"];
        assert_eq!(provider["api_key"], "sk-••••c123");
        assert_eq!(provider["api_keys"][0], "••••");
        assert_eq!(provider["api_keys"][1], "sk-••••cret");
        assert_eq!(provider["base_url"], "https://api.example.com");
    }

    #[test]
    fn mask_json_value_masks_channel_credentials() {
        let v = serde_json::json!({
            "channels": {
                "tg": {
                    "enabled": true,
                    "credentials": { "bot_token": "123456:ABC", "app_id": "987654" }
                }
            }
        });
        let out = mask_json_value(&v);
        assert_eq!(out["channels"]["tg"]["credentials"]["bot_token"], "123••••:ABC");
        // Identifiers inside a credentials container are masked too — the
        // container is treated as an all-secret payload.
        assert_eq!(out["channels"]["tg"]["credentials"]["app_id"], "••••");
        assert_eq!(out["channels"]["tg"]["enabled"], true);
    }

    #[test]
    fn mask_json_value_masks_search_api_key_and_keys() {
        let v = serde_json::json!({
            "search": {
                "provider": "tavily",
                "api_key": "tvly-abc123",
                "keys": { "tavily": "tvly-abc123", "brave": "bsa-xyz" }
            }
        });
        let out = mask_json_value(&v);
        assert_eq!(out["search"]["api_key"], "tvl••••c123");
        assert_eq!(out["search"]["keys"]["tavily"], "tvl••••c123");
        assert_eq!(out["search"]["keys"]["brave"], "bsa••••-xyz");
        assert_eq!(out["search"]["provider"], "tavily");
    }

    #[test]
    fn mask_json_value_env_is_per_key() {
        let v = serde_json::json!({
            "servers": {
                "s": { "env": { "ANTHROPIC_API_KEY": "sk-ant-zzz", "HOST": "localhost", "PORT": "8080", "BASE_URL": "https://x" } }
            }
        });
        let out = mask_json_value(&v);
        assert_eq!(out["servers"]["s"]["env"]["ANTHROPIC_API_KEY"], "sk-••••-zzz");
        assert_eq!(out["servers"]["s"]["env"]["HOST"], "localhost");
        assert_eq!(out["servers"]["s"]["env"]["PORT"], "8080");
        assert_eq!(out["servers"]["s"]["env"]["BASE_URL"], "https://x");
    }

    #[test]
    fn mask_json_value_masks_security_but_not_credential_precedence() {
        let v = serde_json::json!({
            "security": {
                "shared_token": "sh-abc",
                "credential_precedence": "env_first",
                "rate_limit": { "max_per_minute": 10 }
            },
            "oauth": { "client_secret": "cs-secret", "client_id": "cid" }
        });
        let out = mask_json_value(&v);
        assert_eq!(out["security"]["shared_token"], "••••"); // len 6 → fully masked
                                                             // The critical negative: a string whose key merely contains
                                                             // "credential" must not be masked.
        assert_eq!(out["security"]["credential_precedence"], "env_first");
        assert_eq!(out["security"]["rate_limit"]["max_per_minute"], 10);
        assert_eq!(out["oauth"]["client_secret"], "cs-••••cret");
        assert_eq!(out["oauth"]["client_id"], "cid");
    }

    #[test]
    fn mask_json_value_preserves_innocent_config() {
        let v = serde_json::json!({
            "model": "claude-sonnet-4-6",
            "model_provider": "anthropic",
            "default_agent": { "temperature": 0.7, "max_tokens": 4096, "workspace_only": false },
            "heartbeat": { "enabled": true, "interval_seconds": 3600 },
            "agents": { "default": { "token_url": "https://idp", "key_path": "/tmp/k" } }
        });
        assert_eq!(mask_json_value(&v), v);
    }
}
