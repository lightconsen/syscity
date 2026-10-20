//! Auth commands for Syscity
//!
//! Manage the gateway's shared auth token and auth mode:
//!
//! ```text
//! syscity auth token generate|rotate|reset
//! syscity auth show
//! ```
//!
//! Creating a token switches the gateway to Token mode and enables auth
//! enforcement; resetting the token returns the gateway to anonymous (None)
//! mode. Changes are written to the config file, which the daemon's
//! hot-reload watcher picks up automatically — no manual reload needed.

use std::path::{Path, PathBuf};

use clap::Subcommand;

use crate::error::Result;
use crate::gateway::protocol::AuthMode;
use crate::gateway::CredentialPrecedence;

/// Environment variable that can override `security.shared_token`.
const ENV_TOKEN_VAR: &str = "SYSCITY_SECURITY_SHARED_TOKEN";

#[derive(Debug, Subcommand)]
pub enum AuthCommands {
    /// Show the current auth mode, token status, and effective enforcement
    Show {
        /// Path to configuration file
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// Manage the shared auth token
    Token {
        #[command(subcommand)]
        command: TokenCommands,
    },
}

#[derive(Debug, Subcommand)]
pub enum TokenCommands {
    /// Generate a new token and switch to Token mode
    Generate {
        /// Path to configuration file
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// Rotate the token (generate a fresh one) and keep Token mode
    Rotate {
        /// Path to configuration file
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// Clear the token and switch back to anonymous (none) mode
    Reset {
        /// Path to configuration file
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
}

/// Run auth commands
pub async fn run_auth_command(command: &AuthCommands) -> Result<()> {
    match command {
        AuthCommands::Show { file } => show_auth(file.as_ref()).await,
        AuthCommands::Token { command } => match command {
            TokenCommands::Generate { file } => write_token("Generated", file.as_ref()).await,
            TokenCommands::Rotate { file } => write_token("Rotated", file.as_ref()).await,
            TokenCommands::Reset { file } => reset_token(file.as_ref()).await,
        },
    }
}

/// Generate a URL-safe base64 token from 24 bytes of OS entropy (~192 bits).
///
/// URL-safe (no `+`, `/`, `=`) so the token survives both `Bearer` headers
/// and the `auth.token` WebSocket query parameter without escaping issues.
fn generate_token() -> String {
    use base64::Engine as _;

    let mut bytes = [0u8; 24];
    crate::utils::fill_os_entropy(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Write a fresh token, switch to Token mode, and require auth.
///
/// Used by both `generate` and `rotate`. The write order keeps every
/// intermediate state valid for the hot-reload watcher: token first (mode is
/// still `none`, so it only warns), then mode, then enforcement.
async fn write_token(verb: &str, file: Option<&PathBuf>) -> Result<()> {
    let token = generate_token();
    let path = resolve_path(file);

    crate::cli::config_cmd::set_config_value(&format!("security.shared_token={}", token), file)
        .await?;
    crate::cli::config_cmd::set_config_value("security.auth_mode=token", file).await?;
    crate::cli::config_cmd::set_config_value("security.auth_required=true", file).await?;

    println!("{} new token (shown once):", verb);
    println!("  {}", token);
    println!("Auth mode switched to: token");
    println!("Auth required: true");

    warn_env_shadow(&path).await;
    validate_security_config(file).await?;
    Ok(())
}

/// Clear the token and return to anonymous (None) mode.
///
/// Idempotent: when the config file is already anonymous it does not rewrite
/// the file. Write order keeps every intermediate state valid: drop the mode
/// first (token still present, so validation passes), then relax enforcement,
/// then remove the token.
async fn reset_token(file: Option<&PathBuf>) -> Result<()> {
    let (mode_none, has_file_token) = current_auth_state(file).await?;

    if mode_none && !has_file_token {
        println!(
            "Already in anonymous mode (auth_mode=none, no shared_token in config). \
             Nothing to change in the config file."
        );
    } else {
        crate::cli::config_cmd::set_config_value("security.auth_mode=none", file).await?;
        crate::cli::config_cmd::set_config_value("security.auth_required=false", file).await?;
        if has_file_token {
            crate::cli::config_cmd::unset_config_value("security.shared_token", file).await?;
        }

        println!("Auth mode switched to: none (anonymous)");
        println!("Auth required: false");
        println!("Shared token: cleared");
    }

    if env_token().is_some() {
        println!(
            "Warning: {} is still set in the environment; the daemon will keep reading that \
             token. Unset it to fully return to anonymous access.",
            ENV_TOKEN_VAR
        );
    }
    validate_security_config(file).await?;
    Ok(())
}

/// Print the effective auth state without depending on the daemon.
async fn show_auth(file: Option<&PathBuf>) -> Result<()> {
    let path = resolve_path(file);
    let gc = load_gateway_config(&path).await;
    let effective = effective_auth(&gc, &path).await;

    println!("Auth mode:          {}", auth_mode_str(gc.security.auth_mode));
    println!("Auth required:      {}", gc.security.auth_required);
    println!("Security enabled:   {}", gc.security.enabled);
    match &effective.token {
        Some(t) => {
            println!("Shared token:       set · {} (source: {})", mask_token(t), effective.source)
        }
        None => println!("Shared token:       unset"),
    }
    println!("Config file:        {}", path.display());
    println!("Daemon:             {}", daemon_status());

    let mut warnings = Vec::new();
    if env_token().is_some() {
        let note = if effective.source == "environment" {
            format!(
                "{} overrides the config token ({} precedence)",
                ENV_TOKEN_VAR,
                precedence_str(gc.security.credential_precedence)
            )
        } else {
            format!(
                "{} is set but ignored ({} precedence)",
                ENV_TOKEN_VAR,
                precedence_str(gc.security.credential_precedence)
            )
        };
        warnings.push(note);
    }
    if gc.security.auth_mode == AuthMode::Token && effective.token.is_none() {
        warnings.push("auth_mode is 'token' but no shared_token is configured".into());
    }
    if gc.security.auth_mode == AuthMode::None && gc.security.auth_required {
        warnings.push(
            "auth_mode is 'none' but auth_required is true; no auth mechanism will be enforced"
                .into(),
        );
    }

    if warnings.is_empty() {
        println!("\nNo warnings.");
    } else {
        println!("\nWarnings:");
        for w in &warnings {
            println!("  - {}", w);
        }
    }
    Ok(())
}

/// Read the current mode + token-presence directly from the config TOML
/// (config-file only, ignoring env overrides — used for reset idempotency).
async fn current_auth_state(file: Option<&PathBuf>) -> Result<(bool, bool)> {
    let path = resolve_path(file);
    if !path.exists() {
        return Ok((true, false));
    }
    let content = tokio::fs::read_to_string(&path).await?;
    let value: toml::Value = toml::from_str(&content)?;
    let security = value.get("security");
    let mode_none = security
        .and_then(|s| s.get("auth_mode"))
        .and_then(|m| m.as_str())
        .map(|m| m == "none")
        .unwrap_or(true);
    let has_token = security
        .and_then(|s| s.get("shared_token"))
        .and_then(|t| t.as_str())
        .map(|t| !t.is_empty())
        .unwrap_or(false);
    Ok((mode_none, has_token))
}

/// Reload the gateway config and re-run auth validation so the written state
/// is verified before the command returns.
async fn validate_security_config(file: Option<&PathBuf>) -> Result<()> {
    let path = resolve_path(file);
    let gc = load_gateway_config(&path).await;
    crate::gateway::validate_auth_config(&gc)?;
    println!("✅ Security configuration valid");
    Ok(())
}

/// Resolve the config file path (explicit override or the default location).
fn resolve_path(file: Option<&PathBuf>) -> PathBuf {
    file.cloned()
        .unwrap_or_else(crate::dirs::default_config_file)
}

/// Load the gateway config from `path`, applying env credential overrides the
/// same way the daemon does so validation and display match runtime behavior.
///
/// A partial file (e.g. a freshly-created `[security]` section) does not
/// deserialize into the full `GatewayConfig`, so recover the `security` table
/// on top of the defaults instead of silently reporting the default state.
async fn load_gateway_config(path: &Path) -> crate::gateway::GatewayConfig {
    let content = match tokio::fs::read_to_string(path).await {
        Ok(content) => content,
        Err(_) => {
            let mut defaults = crate::gateway::GatewayConfig::default();
            apply_env_security_overrides(&mut defaults);
            return defaults;
        }
    };
    let mut config =
        toml::from_str::<crate::gateway::GatewayConfig>(&content).unwrap_or_else(|_| {
            let mut defaults = crate::gateway::GatewayConfig::default();
            if let Ok(value) = content.parse::<toml::Value>() {
                if let Some(sec) = value.get("security") {
                    if let Ok(sec_toml) = toml::to_string(sec) {
                        if let Ok(sec_cfg) =
                            toml::from_str::<crate::gateway::SecurityConfig>(&sec_toml)
                        {
                            defaults.security = sec_cfg;
                        }
                    }
                }
            }
            defaults
        });
    apply_env_security_overrides(&mut config);
    config
}

/// Effective token + source, mirroring the daemon's credential precedence.
async fn effective_auth(gc: &crate::gateway::GatewayConfig, _path: &Path) -> EffectiveAuth {
    let env_tok = env_token();
    let source = if env_tok.is_some() {
        match gc.security.credential_precedence {
            CredentialPrecedence::EnvFirst => "environment",
            CredentialPrecedence::ConfigFirst
                if gc.security.shared_token.as_deref() == env_tok.as_deref() =>
            {
                "environment"
            }
            CredentialPrecedence::ConfigFirst => "config file",
        }
    } else {
        "config file"
    };
    EffectiveAuth {
        token: gc.security.shared_token.clone(),
        source,
    }
}

struct EffectiveAuth {
    token: Option<String>,
    source: &'static str,
}

/// Apply `SYSCITY_SECURITY_SHARED_TOKEN` overrides, respecting
/// `security.credential_precedence`. Mirrors `daemon::apply_env_security_overrides`.
fn apply_env_security_overrides(config: &mut crate::gateway::GatewayConfig) {
    if let Ok(token) = std::env::var(ENV_TOKEN_VAR) {
        let config_empty = config
            .security
            .shared_token
            .as_ref()
            .map(|s| s.is_empty())
            .unwrap_or(true);
        match config.security.credential_precedence {
            CredentialPrecedence::EnvFirst => {
                config.security.shared_token = Some(token);
            }
            CredentialPrecedence::ConfigFirst if config_empty => {
                config.security.shared_token = Some(token);
            }
            CredentialPrecedence::ConfigFirst => {}
        }
    }
}

fn env_token() -> Option<String> {
    std::env::var(ENV_TOKEN_VAR).ok().filter(|t| !t.is_empty())
}

/// Print a note when an env token may shadow the just-written config token.
async fn warn_env_shadow(path: &Path) {
    if env_token().is_none() {
        return;
    }
    let gc = load_gateway_config(path).await;
    match gc.security.credential_precedence {
        CredentialPrecedence::EnvFirst => {
            println!(
                "Warning: {} is set; with EnvFirst precedence it overrides the config token. \
                 Unset it for the new token to take effect.",
                ENV_TOKEN_VAR
            );
        }
        CredentialPrecedence::ConfigFirst => {
            println!(
                "Note: {} is set but {} precedence means the config token wins.",
                ENV_TOKEN_VAR,
                precedence_str(gc.security.credential_precedence)
            );
        }
    }
}

fn precedence_str(p: CredentialPrecedence) -> &'static str {
    match p {
        CredentialPrecedence::EnvFirst => "EnvFirst",
        CredentialPrecedence::ConfigFirst => "ConfigFirst",
    }
}

fn auth_mode_str(mode: AuthMode) -> &'static str {
    match mode {
        AuthMode::None => "none (anonymous)",
        AuthMode::Token => "token",
        AuthMode::Device => "device",
        AuthMode::Tailscale => "tailscale",
    }
}

fn mask_token(token: &str) -> String {
    if token.len() <= 4 {
        "*".repeat(token.len())
    } else {
        format!("****{}", &token[token.len() - 4..])
    }
}

fn daemon_status() -> String {
    let pid_path = crate::dirs::pid_file();
    if !pid_path.exists() {
        return "not running".to_string();
    }
    match std::fs::read_to_string(&pid_path) {
        Ok(text) if !text.trim().is_empty() => format!("running (pid {})", text.trim()),
        _ => "running (pid file present)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_config() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        (dir, path)
    }

    async fn read_security(path: &Path) -> toml::Value {
        let content = tokio::fs::read_to_string(path).await.unwrap();
        let value: toml::Value = toml::from_str(&content).unwrap();
        value
            .get("security")
            .cloned()
            .unwrap_or(toml::Value::Table(Default::default()))
    }

    fn field<'a>(security: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
        security.get(key)
    }

    #[tokio::test]
    async fn generate_writes_token_and_enables_token_mode() {
        let (_dir, path) = temp_config();
        write_token("Generated", Some(&path)).await.unwrap();

        let sec = read_security(&path).await;
        assert_eq!(field(&sec, "auth_mode").and_then(|v| v.as_str()), Some("token"));
        assert_eq!(field(&sec, "auth_required").and_then(|v| v.as_bool()), Some(true));
        let token = field(&sec, "shared_token")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(token.len() == 32);
        assert!(token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[tokio::test]
    async fn rotate_changes_existing_token_keeps_token_mode() {
        let (_dir, path) = temp_config();
        write_token("Generated", Some(&path)).await.unwrap();
        let first = read_security(&path).await;
        let token1 = field(&first, "shared_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();

        write_token("Rotated", Some(&path)).await.unwrap();
        let second = read_security(&path).await;
        let token2 = field(&second, "shared_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();

        assert_ne!(token1, token2);
        assert_eq!(field(&second, "auth_mode").and_then(|v| v.as_str()), Some("token"));
        assert_eq!(field(&second, "auth_required").and_then(|v| v.as_bool()), Some(true));
    }

    #[tokio::test]
    async fn reset_clears_token_and_returns_to_anonymous() {
        let (_dir, path) = temp_config();
        write_token("Generated", Some(&path)).await.unwrap();

        reset_token(Some(&path)).await.unwrap();
        let sec = read_security(&path).await;
        assert_eq!(field(&sec, "auth_mode").and_then(|v| v.as_str()), Some("none"));
        assert_eq!(field(&sec, "auth_required").and_then(|v| v.as_bool()), Some(false));
        assert!(field(&sec, "shared_token").is_none());
    }

    #[tokio::test]
    async fn reset_is_idempotent() {
        // Reset on a fresh (missing) config is a no-op, not an error.
        let (_dir, path) = temp_config();
        reset_token(Some(&path)).await.unwrap();

        // And a full generate -> reset -> reset cycle stays clean.
        write_token("Generated", Some(&path)).await.unwrap();
        reset_token(Some(&path)).await.unwrap();
        reset_token(Some(&path)).await.unwrap();
        let sec = read_security(&path).await;
        assert_eq!(field(&sec, "auth_mode").and_then(|v| v.as_str()), Some("none"));
        assert_eq!(field(&sec, "auth_required").and_then(|v| v.as_bool()), Some(false));
        assert!(field(&sec, "shared_token").is_none());
    }

    #[test]
    fn token_uses_url_safe_alphabet() {
        let token = generate_token();
        assert_eq!(token.len(), 32);
        assert!(token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
