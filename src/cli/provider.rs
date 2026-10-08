//! Provider management commands for Syscity
//!
//! Top-level CLI for listing, enabling, disabling, and switching model
//! providers (over WebSocket).

use clap::Subcommand;
use serde_json::json;

use crate::cli::ws;
use crate::error::Result;

#[derive(Debug, Subcommand)]
pub enum ProviderCommands {
    /// List available LLM providers
    List,
    /// Show provider health status
    Health {
        /// Provider ID
        id: String,
    },
    /// Enable a provider
    Enable {
        /// Provider ID
        id: String,
    },
    /// Disable a provider
    Disable {
        /// Provider ID
        id: String,
    },
    /// Switch the default model
    Switch {
        /// Concrete model ID to switch to
        model: String,
    },
    /// Show current default model
    Default,
    /// Show provider usage statistics
    Usage {
        /// Provider ID (omit for all providers)
        id: Option<String>,
    },
    /// Authorize a provider via OAuth 2.0 (authorization code + PKCE)
    ///
    /// The flow runs on the gateway: the authorization URL comes from
    /// `providers.auth_start`, the provider redirects the browser to the
    /// gateway's own `/oauth/provider/callback`, and the refresh token is
    /// stored in the secret store — it is never printed. The flow parameters
    /// come from `[providers.<id>.oauth]` in the config.
    Auth {
        /// Provider ID (must have an `[providers.<id>.oauth]` block)
        id: String,
        /// Seconds to wait for the authorization to complete (default: 300)
        #[arg(long, default_value = "300")]
        timeout: u64,
        /// Don't open a browser automatically
        #[arg(long)]
        no_browser: bool,
    },
}

/// Run provider commands (over WebSocket).
pub async fn run_provider_command(
    command: &ProviderCommands,
    _config: &crate::config::Config,
) -> Result<()> {
    match command {
        ProviderCommands::List => {
            let payload = ws::call("providers.list", json!({})).await?;
            if let Some(providers) = payload.get("providers").and_then(|p| p.as_array()) {
                println!("Providers:");
                println!("{:<20} {:<10} {:<10} Name", "ID", "Enabled", "Healthy");
                println!("{}", "-".repeat(60));
                for p in providers {
                    println!(
                        "{:<20} {:<10} {:<10} {}",
                        p.get("id").and_then(|c| c.as_str()).unwrap_or("-"),
                        if p.get("enabled").and_then(|c| c.as_bool()).unwrap_or(false) {
                            "yes"
                        } else {
                            "no"
                        },
                        if p.get("healthy").and_then(|c| c.as_bool()).unwrap_or(false) {
                            "yes"
                        } else {
                            "no"
                        },
                        p.get("name").and_then(|c| c.as_str()).unwrap_or("-"),
                    );
                }
            }
            Ok(())
        }
        ProviderCommands::Health { id } => {
            let payload = ws::call("providers.health", json!({ "id": id })).await?;
            println!("{}", payload);
            Ok(())
        }
        ProviderCommands::Enable { id } => {
            match ws::call("providers.enable", json!({ "id": id })).await {
                Ok(_) => println!("✅ Enabled provider {}", id),
                Err(e) => {
                    eprintln!("Failed to enable: {}", e);
                    return Err(e);
                }
            }
            Ok(())
        }
        ProviderCommands::Disable { id } => {
            match ws::call("providers.disable", json!({ "id": id })).await {
                Ok(_) => println!("✅ Disabled provider {}", id),
                Err(e) => {
                    eprintln!("Failed to disable: {}", e);
                    return Err(e);
                }
            }
            Ok(())
        }
        ProviderCommands::Switch { model } => {
            match ws::call("providers.switch", json!({ "model": model })).await {
                Ok(_) => println!("✅ Switched default model to {}", model),
                Err(e) => {
                    eprintln!("Failed to switch: {}", e);
                    return Err(e);
                }
            }
            Ok(())
        }
        ProviderCommands::Default => {
            let payload = ws::call("models.default", json!({})).await?;
            println!("{}", payload);
            Ok(())
        }
        ProviderCommands::Usage { id } => {
            let payload = if let Some(ref provider_id) = id {
                ws::call("providers.usage", json!({ "id": provider_id })).await?
            } else {
                ws::call("providers.usage", json!({})).await?
            };
            // Try to parse as formatted usage snapshots
            let snapshots_value = payload.get("usage").cloned().unwrap_or(payload);
            if let Ok(snapshots) = serde_json::from_value::<
                Vec<crate::model_router::ProviderUsageSnapshot>,
            >(snapshots_value.clone())
            {
                let fmt_config = crate::model_router::usage_formatter::FormatConfig::default();
                if id.is_some() {
                    for snapshot in &snapshots {
                        println!(
                            "{}",
                            crate::model_router::format_provider_snapshot(snapshot, &fmt_config)
                        );
                    }
                } else {
                    println!(
                        "{}",
                        crate::model_router::format_usage_report(&snapshots, &fmt_config)
                    );
                }
            } else {
                // Fallback to pretty-printed JSON
                println!("{}", serde_json::to_string_pretty(&snapshots_value).unwrap_or_default());
            }
            Ok(())
        }
        ProviderCommands::Auth { id, timeout, no_browser } => {
            run_auth_command(id, *timeout, *no_browser).await
        }
    }
}

/// Authorize a provider, driving the gateway's OAuth flow.
///
/// Everything happens on the gateway: this asks for the URL, opens it, and
/// waits for `providers.auth_status` to report the result. No token comes back
/// through here — the credential is stored on the gateway side, which is also
/// what makes the flow work when the browser and the gateway are not on the
/// same machine (a loopback listener cannot).
async fn run_auth_command(provider_id: &str, timeout_secs: u64, no_browser: bool) -> Result<()> {
    let started = ws::call("providers.auth_start", json!({ "id": provider_id })).await?;
    let authorization_url = started
        .get("auth_url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            crate::error::SyscityError::Internal(
                "providers.auth_start returned no auth_url".to_string(),
            )
        })?
        .to_string();

    println!("\n🔐  OAuth authorization for '{provider_id}'\n");
    println!("Open this URL in your browser:\n");
    println!("  {authorization_url}\n");

    if !no_browser {
        open_in_browser(&authorization_url);
    }

    println!("Waiting for the authorization to complete (timeout: {timeout_secs}s)...\n");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(crate::error::SyscityError::Timeout(format!(
                "'{provider_id}' was not authorized within {timeout_secs}s. The pending flow \
                 expires on its own — run this again when you are ready"
            )));
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let status = ws::call("providers.auth_status", json!({ "id": provider_id })).await?;
        if status
            .get("authorized")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            println!("✅  '{provider_id}' is now authorized.\n");
            return Ok(());
        }
        // `pending` goes null when the flow is gone. Success is handled above,
        // so this means it was refused or expired on the gateway.
        if status.get("pending").map(|v| v.is_null()).unwrap_or(false) {
            return Err(crate::error::SyscityError::Validation(format!(
                "the authorization for '{provider_id}' did not complete (refused, or the flow \
                 expired). Run this again to retry"
            )));
        }
    }
}

/// Hand the authorization URL to the desktop's browser opener.
///
/// Best effort: the URL is printed either way, and a browser running somewhere
/// else (a headless gateway host) has to open it by hand.
fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let spawned = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let spawned = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    if let Err(e) = spawned {
        println!("(could not open a browser automatically: {e})");
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let _ = url;
}
