use serde::{Deserialize, Serialize};

/// Syscity Cloud integration config (§2.7 / docs/cloud-integration.md).
///
/// Runtime gate: only active when `enabled = true` **and** a session token is
/// present (double gate). The session token itself lives in the SecretStore,
/// not in config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudConfig {
    /// Master runtime switch for the cloud path.
    ///
    /// Defaults to **on** for a binary compiled with the `cloud` feature
    /// (this module only exists then); the actual cloud paths still require a
    /// logged-in session, so an anonymous user just sees the local behavior.
    /// Turn cloud off with `syscity start --nocloud`, `SYSCITY_CLOUD_ENABLED=0`,
    /// or config.toml `[cloud] enabled = false`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Cloud API base (OpenAI-compatible `/v1/*` + `/api/v1/*`).
    #[serde(default = "default_api_base")]
    pub api_base: String,
    /// Engine web URL the cloud OAuth callback lands on after login.
    #[serde(default = "default_redirect_base")]
    pub redirect_base: String,
    /// The cloud **console** origin hosting the provider-chooser login page
    /// (e.g. `http://localhost:5173` in dev). Sign-in redirects here so the
    /// user picks GitHub/Google/WeChat on the cloud, instead of the engine
    /// hardcoding a provider. The console's `/login?redirect=…` passes the
    /// engine callback through to the chosen provider's OAuth.
    #[serde(default = "default_console_url")]
    pub console_url: String,
}

fn default_api_base() -> String {
    "https://api.syscity.net".to_string()
}

/// Env-aware default for `enabled`, shared by the serde field default and
/// `Default`. Cloud is on unless `SYSCITY_CLOUD_ENABLED` is set to something
/// other than `1`/`true`. Using a named fn (instead of plain `#[serde(default)]`,
/// which would yield `bool::default() == false`) keeps a `[cloud]` section
/// without an explicit `enabled` key on the same on-by-default behavior as a
/// completely absent section.
fn default_enabled() -> bool {
    match std::env::var("SYSCITY_CLOUD_ENABLED") {
        Ok(v) => v == "1" || v.eq_ignore_ascii_case("true"),
        Err(_) => true,
    }
}

fn default_redirect_base() -> String {
    "http://localhost:18080/cloud/login/callback".to_string()
}

fn default_console_url() -> String {
    // The console SPA (provider-chooser login page) is deployed at
    // cloud.syscity.net — the API host (api.syscity.net) serves no HTML.
    // Dev setups override with SYSCITY_CLOUD_CONSOLE_URL (e.g.
    // http://localhost:5173).
    "https://cloud.syscity.net".to_string()
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            // A cloud-compiled binary is a deliberate choice to ship cloud
            // support — default it on, still overridable via env.
            enabled: default_enabled(),
            api_base: std::env::var("SYSCITY_CLOUD_API_BASE")
                .unwrap_or_else(|_| default_api_base()),
            redirect_base: std::env::var("SYSCITY_CLOUD_REDIRECT_BASE")
                .unwrap_or_else(|_| default_redirect_base()),
            console_url: std::env::var("SYSCITY_CLOUD_CONSOLE_URL")
                .unwrap_or_else(|_| default_console_url()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_missing_enabled_key_defaults_to_env_aware_on() {
        // A `[cloud]` section without an explicit `enabled` key must land on
        // the same value as `Default` (on when the env override is absent) —
        // a plain `#[serde(default)]` would deserialize to `false` here.
        let section: CloudConfig = toml::from_str("api_base = \"https://example.com\"").unwrap();
        assert_eq!(section.enabled, CloudConfig::default().enabled);

        // Whole config with no cloud section at all: same story.
        let empty: CloudConfig = toml::from_str("").unwrap();
        assert_eq!(empty.enabled, CloudConfig::default().enabled);
    }

    #[test]
    fn test_explicit_enabled_false_stays_off() {
        let off: CloudConfig = toml::from_str("enabled = false").unwrap();
        assert!(!off.enabled);

        let on: CloudConfig = toml::from_str("enabled = true").unwrap();
        assert!(on.enabled);
    }
}
