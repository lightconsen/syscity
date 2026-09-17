//! Doctor diagnostic system for Syscity
//!
//! Provides `syscity doctor run` and `syscity doctor report` commands for
//! diagnosing provider health, auth status, circuit state, and generating
//! actionable recommendations.

use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::cli::ws;
use crate::error::{Result, SyscityError};

/// Default daemon base URL.
const DAEMON_URL: &str = "http://127.0.0.1:18080";

/// Path to cache the last diagnostic report.
fn report_cache_path() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("syscity")
        .join("last_doctor_report.json")
}

/// Doctor subcommands.
#[derive(Debug, Subcommand)]
pub enum DoctorCommands {
    /// Run diagnostics against the daemon
    Run {
        /// Filter to a specific provider
        #[arg(short, long)]
        provider: Option<String>,
        /// Show verbose output
        #[arg(short, long)]
        verbose: bool,
    },
    /// Show the last diagnostic report
    Report,
}

/// Overall health grade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HealthGrade {
    Healthy,
    Degraded,
    Critical,
}

impl std::fmt::Display for HealthGrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HealthGrade::Healthy => write!(f, "Healthy"),
            HealthGrade::Degraded => write!(f, "Degraded"),
            HealthGrade::Critical => write!(f, "Critical"),
        }
    }
}

/// Diagnostic result for a single provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDiagnostic {
    pub provider: String,
    pub health: HealthGrade,
    pub circuit_state: String,
    pub auth_status: String,
    pub usage_status: String,
    pub last_error: Option<String>,
    pub recommendation: Option<String>,
}

/// Full doctor report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub overall_health: HealthGrade,
    pub provider_diagnostics: Vec<ProviderDiagnostic>,
    pub auth_diagnostics: Vec<AuthDiagnostic>,
    pub deprecation_warnings: Vec<String>,
    pub migration_hints: Vec<String>,
    pub plugin_hints: Vec<DiagnosticHint>,
    pub recommendations: Vec<String>,
    pub timestamp: String,
}

/// Auth diagnostic info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthDiagnostic {
    pub provider: String,
    pub total_keys: usize,
    pub available_keys: usize,
    pub status: String,
}

/// Deprecation rule for detecting outdated providers/models.
#[derive(Debug, Clone)]
struct DeprecationRule {
    /// Pattern to match against model id or provider name
    pattern: &'static str,
    /// Human-readable reason for deprecation
    reason: &'static str,
    /// Suggested migration action. Owned because the suggestion names the
    /// current default (`providers::DEFAULT_MODEL`), which must not be
    /// duplicated here as a literal that goes stale.
    migration: String,
}

/// Deprecated models and what to move to.
///
/// A function rather than a `static`: the migration text embeds the current
/// default model, and a literal here would be one more place to keep in step.
fn deprecation_rules() -> Vec<DeprecationRule> {
    let current = crate::providers::DEFAULT_MODEL.to_string();
    vec![
        DeprecationRule {
            pattern: "gpt-3.5-turbo",
            reason: "OpenAI GPT-3.5 Turbo is deprecated",
            migration: "Switch to 'gpt-4o-mini' for better cost and performance".to_string(),
        },
        DeprecationRule {
            pattern: "gpt-3.5",
            reason: "OpenAI GPT-3.5 family is deprecated",
            migration: "Switch to 'gpt-4o-mini' for better cost and performance".to_string(),
        },
        DeprecationRule {
            pattern: "claude-2",
            reason: "Anthropic Claude 2 is deprecated",
            migration: format!("Switch to '{current}' for better performance"),
        },
        DeprecationRule {
            pattern: "text-davinci",
            reason: "OpenAI Davinci models are deprecated",
            migration: "Switch to GPT-4o or GPT-4o-mini".to_string(),
        },
        DeprecationRule {
            pattern: "code-davinci",
            reason: "OpenAI Codex models are deprecated",
            migration: "Switch to GPT-4o with coding tasks".to_string(),
        },
    ]
}

/// Extension point for plugin-provided diagnostics.
pub trait DoctorPlugin: Send + Sync {
    /// Plugin name
    #[allow(dead_code)]
    fn name(&self) -> &str;
    /// Run plugin-specific diagnostics against a provider
    #[allow(dead_code)]
    fn diagnose(
        &self,
        provider: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<DiagnosticHint>> + Send + '_>>;
}

/// A single diagnostic hint from a plugin or built-in checker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticHint {
    pub category: String,
    pub message: String,
    pub severity: HintSeverity,
}

/// Severity of a diagnostic hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HintSeverity {
    Info,
    Warning,
    Error,
}

/// Run doctor commands.
pub async fn run_doctor_command(command: &DoctorCommands) -> Result<()> {
    match command {
        DoctorCommands::Run { provider, verbose } => {
            let report = run_diagnostics(provider.as_deref(), *verbose).await?;
            print_report(&report, *verbose);

            // Cache report to disk
            if let Ok(json) = serde_json::to_string_pretty(&report) {
                let path = report_cache_path();
                if let Some(parent) = path.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
                let _ = tokio::fs::write(&path, json).await;
            }

            Ok(())
        }
        DoctorCommands::Report => {
            let path = report_cache_path();
            let contents = match tokio::fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(_) => {
                    println!("No diagnostic report found. Run `syscity doctor run` first.");
                    return Ok(());
                }
            };
            let report: DoctorReport = serde_json::from_str(&contents)
                .map_err(|e| SyscityError::Internal(format!("Failed to parse report: {}", e)))?;
            print_report(&report, false);
            Ok(())
        }
    }
}

async fn run_diagnostics(filter_provider: Option<&str>, verbose: bool) -> Result<DoctorReport> {
    let mut provider_diagnostics = Vec::new();
    let mut auth_diagnostics = Vec::new();
    let mut deprecation_warnings = Vec::new();
    let mut migration_hints = Vec::new();
    let mut plugin_hints = Vec::new();
    let mut recommendations = Vec::new();

    // Fetch provider list
    let providers_resp = ws::call("providers.list", serde_json::json!({})).await;

    let providers: Vec<serde_json::Value> = match providers_resp {
        Ok(payload) => payload
            .get("providers")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default(),
        Err(e) => {
            return Ok(DoctorReport {
                overall_health: HealthGrade::Critical,
                provider_diagnostics: Vec::new(),
                auth_diagnostics: Vec::new(),
                deprecation_warnings: Vec::new(),
                migration_hints: Vec::new(),
                plugin_hints: Vec::new(),
                recommendations: vec![format!(
                    "Cannot reach daemon at {}: {}. Is syscity running?",
                    DAEMON_URL, e
                )],
                timestamp: chrono::Utc::now().to_rfc3339(),
            });
        }
    };

    for p in &providers {
        let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");

        if let Some(filter) = filter_provider {
            if id != filter {
                continue;
            }
        }

        let name = p.get("name").and_then(|v| v.as_str()).unwrap_or(id);
        let enabled = p.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
        let healthy = p.get("healthy").and_then(|v| v.as_bool()).unwrap_or(false);

        // Health check endpoint
        let health_body: Option<JsonValue> =
            ws::call("providers.health", serde_json::json!({ "id": id }))
                .await
                .ok();

        let circuit_state = health_body
            .as_ref()
            .and_then(|b| b.get("circuit_state").and_then(|v| v.as_str()))
            .unwrap_or("unknown")
            .to_string();

        // Auth status
        let (auth_status, auth_body) =
            match ws::call("providers.check", serde_json::json!({ "id": id })).await {
                Ok(payload) => ("ok".to_string(), Some(payload)),
                Err(_) => ("unreachable".to_string(), None),
            };

        // Usage status
        let usage_body: Option<JsonValue> =
            ws::call("providers.usage", serde_json::json!({ "id": id }))
                .await
                .ok();

        let usage_status = usage_body
            .as_ref()
            .map(|b| {
                let total = b
                    .get("total_tokens")
                    .and_then(|v| v.get("total_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if total > 0 {
                    format!("{} tokens", total)
                } else {
                    "no usage".to_string()
                }
            })
            .unwrap_or_else(|| "unknown".to_string());

        let mut health = HealthGrade::Healthy;
        let mut last_error = None;
        let mut recommendation = None;

        if !enabled {
            health = HealthGrade::Degraded;
            recommendation = Some(format!(
                "Provider '{}' is disabled — run `syscity provider enable {}`",
                name, id
            ));
        } else if !healthy {
            health = HealthGrade::Critical;
            recommendation =
                Some(format!("Provider '{}' is unhealthy — check API key and network", name));
        } else if circuit_state == "Open" {
            health = HealthGrade::Degraded;
            recommendation = Some(format!(
                "Provider '{}' circuit breaker is open — wait for cooldown or investigate failures",
                name
            ));
        } else if auth_status != "ok" {
            health = HealthGrade::Degraded;
            recommendation =
                Some(format!("Provider '{}' auth check failed — verify credentials", name));
        }

        if verbose {
            if let Some(ref body) = health_body {
                if let Some(err) = body.get("last_error").and_then(|v| v.as_str()) {
                    last_error = Some(err.to_string());
                }
            }
        }

        provider_diagnostics.push(ProviderDiagnostic {
            provider: name.to_string(),
            health,
            circuit_state,
            auth_status,
            usage_status,
            last_error,
            recommendation: recommendation.clone(),
        });

        if let Some(rec) = recommendation {
            recommendations.push(rec);
        }

        // Auth diagnostics
        if let Some(body) = auth_body {
            let total_keys = body.get("total_keys").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let available_keys = body
                .get("available_keys")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let status = if available_keys == 0 && total_keys > 0 {
                "all_keys_disabled".to_string()
            } else if available_keys < total_keys {
                format!("{}/{} keys available", available_keys, total_keys)
            } else {
                "all_keys_available".to_string()
            };

            auth_diagnostics.push(AuthDiagnostic {
                provider: name.to_string(),
                total_keys,
                available_keys,
                status,
            });
        }
    }

    // Deprecation checks: fetch model catalog and match against rules
    match ws::call("models.list", serde_json::json!({})).await {
        Ok(payload) => {
            if let Some(models) = payload.get("models").and_then(|m| m.as_array()) {
                for model in models {
                    let model_id = model.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let provider = model.get("provider").and_then(|v| v.as_str()).unwrap_or("");
                    for rule in deprecation_rules() {
                        if model_id.contains(rule.pattern) || provider.contains(rule.pattern) {
                            let msg = format!(
                                "{} (model: {}, provider: {})",
                                rule.reason, model_id, provider
                            );
                            if !deprecation_warnings.contains(&msg) {
                                deprecation_warnings.push(msg);
                            }
                            let hint =
                                format!("Migrate {}:{} — {}", provider, model_id, rule.migration);
                            if !migration_hints.contains(&hint) {
                                migration_hints.push(hint);
                            }
                        }
                    }
                }
            }
        }
        Err(_) => {
            // Model catalog unavailable — skip deprecation checks
        }
    }

    // Config misconfiguration checks (local config file)
    if let Ok(config) = crate::config::Config::load() {
        // Heartbeat status
        let heartbeat_enabled = config.heartbeat.enabled;
        if heartbeat_enabled {
            recommendations.push(format!(
                "Heartbeat is enabled: interval={}s, active_hours={}-{}",
                config.heartbeat.interval_seconds,
                config.heartbeat.active_hours_start,
                config.heartbeat.active_hours_end,
            ));
        } else if config.heartbeat.interval_seconds > 0 {
            recommendations.push(
                "Heartbeat is configured but disabled — set heartbeat.enabled = true to enable"
                    .to_string(),
            );
        }

        for (service_name, service) in &config.services {
            if let Some(ref key) = service.api_key {
                let key_str = match key {
                    crate::secrets::SecretRef::String(s) => s.as_str(),
                    crate::secrets::SecretRef::Explicit { env: Some(e), .. } => e.as_str(),
                    _ => "",
                };
                if key_str.is_empty() || key_str == "$PLACEHOLDER" {
                    let msg =
                        format!("Service '{}' has empty or placeholder API key", service_name);
                    recommendations.push(msg);
                }
            }
            if service.endpoint.is_empty() {
                recommendations.push(format!("Service '{}' has empty endpoint URL", service_name));
            }
            // Check for common URL misconfigurations
            if !service.endpoint.is_empty()
                && !service.endpoint.starts_with("http://")
                && !service.endpoint.starts_with("https://")
            {
                recommendations.push(format!(
                    "Service '{}' endpoint '{}' is missing http:// or https:// scheme",
                    service_name, service.endpoint
                ));
            }
        }

        // Check memory config for suspicious values
        if config.memory.dreaming.enabled && config.memory.dreaming.frequency.is_empty() {
            recommendations.push("Memory dreaming is enabled but frequency is empty".to_string());
        }
    }

    // Plugin diagnostics — query daemon for plugin list
    let plugins_resp = ws::call("plugins.list", serde_json::json!({})).await;

    if let Ok(payload) = plugins_resp {
        if let Some(plugins) = payload.get("plugins").and_then(|v| v.as_array()) {
            for p in plugins {
                let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                let name = p.get("name").and_then(|v| v.as_str()).unwrap_or(id);
                let enabled = p.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);

                if !enabled {
                    plugin_hints.push(DiagnosticHint {
                        category: format!("plugin:{}", id),
                        message: format!("Plugin '{}' ({}) is disabled", name, id),
                        severity: HintSeverity::Warning,
                    });
                }

                // Check for version info if available
                if let Some(version) = p.get("version").and_then(|v| v.as_str()) {
                    if crate::skills::semver::Version::parse(version).is_err() {
                        plugin_hints.push(DiagnosticHint {
                            category: format!("plugin:{}", id),
                            message: format!(
                                "Plugin '{}' has invalid semver version '{}'",
                                name, version
                            ),
                            severity: HintSeverity::Warning,
                        });
                    }
                }
            }
        }
    }

    // Determine overall health
    let overall_health = if provider_diagnostics
        .iter()
        .any(|d| d.health == HealthGrade::Critical)
    {
        HealthGrade::Critical
    } else if provider_diagnostics
        .iter()
        .any(|d| d.health == HealthGrade::Degraded)
    {
        HealthGrade::Degraded
    } else if provider_diagnostics.is_empty() {
        HealthGrade::Critical
    } else {
        HealthGrade::Healthy
    };

    if provider_diagnostics.is_empty() && filter_provider.is_none() {
        recommendations.push(
            "No providers configured. Run `syscity setup` to configure providers.".to_string(),
        );
    }

    // Add deprecation warnings to recommendations
    for warning in &deprecation_warnings {
        recommendations.push(format!("Deprecation: {}", warning));
    }

    Ok(DoctorReport {
        overall_health,
        provider_diagnostics,
        auth_diagnostics,
        deprecation_warnings,
        migration_hints,
        plugin_hints,
        recommendations,
        timestamp: chrono::Utc::now().to_rfc3339(),
    })
}

fn print_report(report: &DoctorReport, verbose: bool) {
    println!("\nSyscity Diagnostic Report");
    println!("{}", "=".repeat(50));
    println!("Timestamp: {}", report.timestamp);
    println!(
        "Overall Health: {}\n",
        match report.overall_health {
            HealthGrade::Healthy => "✅ Healthy",
            HealthGrade::Degraded => "⚠️  Degraded",
            HealthGrade::Critical => "❌ Critical",
        }
    );

    if !report.provider_diagnostics.is_empty() {
        println!("Provider Diagnostics:");
        println!("{}", "-".repeat(50));
        for d in &report.provider_diagnostics {
            let health_icon = match d.health {
                HealthGrade::Healthy => "✅",
                HealthGrade::Degraded => "⚠️",
                HealthGrade::Critical => "❌",
            };
            println!(
                "{} {:<20} | Circuit: {:<12} | Auth: {:<12} | Usage: {}",
                health_icon, d.provider, d.circuit_state, d.auth_status, d.usage_status
            );
            if verbose {
                if let Some(ref err) = d.last_error {
                    println!("    Last error: {}", err);
                }
                if let Some(ref rec) = d.recommendation {
                    println!("    → {}", rec);
                }
            }
        }
        println!();
    }

    if !report.auth_diagnostics.is_empty() {
        println!("Auth Diagnostics:");
        println!("{}", "-".repeat(50));
        for a in &report.auth_diagnostics {
            println!(
                "  {:<20} | Keys: {}/{} | {}",
                a.provider, a.available_keys, a.total_keys, a.status
            );
        }
        println!();
    }

    if !report.deprecation_warnings.is_empty() {
        println!("Deprecation Warnings:");
        println!("{}", "-".repeat(50));
        for w in &report.deprecation_warnings {
            println!("  ⚠️  {}", w);
        }
        println!();
    }

    if !report.migration_hints.is_empty() {
        println!("Migration Hints:");
        println!("{}", "-".repeat(50));
        for h in &report.migration_hints {
            println!("  → {}", h);
        }
        println!();
    }

    if !report.plugin_hints.is_empty() {
        println!("Plugin Diagnostics:");
        println!("{}", "-".repeat(50));
        for h in &report.plugin_hints {
            let icon = match h.severity {
                HintSeverity::Info => "ℹ️",
                HintSeverity::Warning => "⚠️",
                HintSeverity::Error => "❌",
            };
            println!("  {} [{}] {}", icon, h.category, h.message);
        }
        println!();
    }

    if !report.recommendations.is_empty() {
        println!("Recommendations:");
        println!("{}", "-".repeat(50));
        for (i, rec) in report.recommendations.iter().enumerate() {
            println!("  {}. {}", i + 1, rec);
        }
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deprecation hint names the model the project actually recommends
    /// now — it used to name `claude-3-5-sonnet`, which was older than the
    /// provider's own default.
    #[test]
    fn the_claude_2_migration_names_the_current_default() {
        let rule = deprecation_rules()
            .into_iter()
            .find(|r| r.pattern == "claude-2")
            .expect("claude-2 is on the deprecation list");
        assert!(
            rule.migration.contains(crate::providers::DEFAULT_MODEL),
            "hint should name {}: {}",
            crate::providers::DEFAULT_MODEL,
            rule.migration
        );
    }
}
