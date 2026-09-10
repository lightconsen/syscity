//! Cloud session: OAuth login URL building + session token storage.

use crate::cloud::config::CloudConfig;
use crate::secrets::{SecretId, SecretOrigin, SecretStoreHandle};

pub const CLOUD_NS: &str = "cloud";
pub const ENTITY_SESSION: &str = "session";

fn token_id() -> SecretId {
    SecretId::secret(CLOUD_NS, ENTITY_SESSION)
}

/// The stored cloud session token, if any.
pub async fn get_token(secrets: &SecretStoreHandle) -> Option<String> {
    let value: Option<String> = secrets
        .choose(&token_id())
        .get(&token_id())
        .await
        .ok()
        .flatten();
    value
}

/// Persist a session token (keyring-preferred for user-entered secrets).
pub async fn set_token(secrets: &SecretStoreHandle, token: &str) -> crate::Result<()> {
    secrets
        .choose(&token_id())
        .set(&token_id(), token, SecretOrigin::UserEntered)
        .await
}

/// Forget the session token (revoked / signed out).
pub async fn clear_token(secrets: &SecretStoreHandle) -> crate::Result<()> {
    secrets.choose(&token_id()).delete(&token_id()).await
}

/// Whether a session token is stored.
pub async fn logged_in(secrets: &SecretStoreHandle) -> bool {
    get_token(secrets).await.is_some()
}

/// Cloud login URL: the console's provider-chooser page. The user picks
/// GitHub/Google/WeChat there; the console passes `redirect` (the engine
/// callback) through to the chosen provider's OAuth, which lands back on
/// `config.redirect_base#token=...`.
pub fn login_url(cfg: &CloudConfig, _provider: &str) -> String {
    let redirect = cfg.redirect_base.clone();
    format!(
        "{}/login?redirect={}",
        cfg.console_url.trim_end_matches('/'),
        urlencoding(&redirect)
    )
}

/// Minimal percent-encoding for the `redirect` query param.
fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
