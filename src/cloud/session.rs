//! Cloud session: OAuth login URL building + session token storage.

use crate::cloud::config::CloudConfig;
use crate::secrets::{SecretId, SecretStoreHandle};

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
    secrets.choose(&token_id()).set(&token_id(), token).await
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
///
/// `flow` rides along as a query parameter on the redirect. The cloud appends
/// `#token=` to the redirect verbatim, so the callback page still sees it and
/// knows which UI started the sign-in: `desktop` came from the desktop shell
/// (a real browser tab, no `window.opener`), which must not navigate to the
/// gateway's page afterwards — it shows a "you can close this window" screen
/// instead. It is restricted to the known words by the HTTP handler before it
/// gets here, because the value is spliced into a redirect URL.
pub fn login_url(cfg: &CloudConfig, _provider: &str, flow: Option<&str>) -> String {
    let mut redirect = cfg.redirect_base.clone();
    if let Some(flow) = flow {
        redirect.push(if redirect.contains('?') { '&' } else { '?' });
        redirect.push_str("flow=");
        redirect.push_str(flow);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(redirect_base: &str) -> CloudConfig {
        CloudConfig {
            redirect_base: redirect_base.to_string(),
            console_url: "https://cloud.syscity.net/".to_string(),
            ..CloudConfig::default()
        }
    }

    #[test]
    fn login_url_points_at_the_console() {
        let url = login_url(&cfg("http://localhost:18080/cloud/login/callback"), "github", None);
        // The trailing slash on the console URL is not doubled.
        assert!(url.starts_with("https://cloud.syscity.net/login?redirect="), "{url}");
        assert!(
            url.contains("http%3A%2F%2Flocalhost%3A18080%2Fcloud%2Flogin%2Fcallback"),
            "{url}"
        );
    }

    #[test]
    fn flow_rides_on_the_redirect_not_the_console_page() {
        // The callback page reads the flow out of its own query string, so it
        // has to be inside the encoded `redirect`, not a sibling of it.
        let url = login_url(
            &cfg("http://localhost:18080/cloud/login/callback"),
            "github",
            Some("desktop"),
        );
        assert!(url.contains("callback%3Fflow%3Ddesktop"), "{url}");
    }

    #[test]
    fn flow_appends_to_an_existing_query() {
        let url = login_url(&cfg("http://localhost:18080/cb?x=1"), "github", Some("tab"));
        assert!(url.contains("%3Fx%3D1%26flow%3Dtab"), "{url}");
    }
}
