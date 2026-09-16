//! Security commands for Syscity
//!
//! Provides security audit, DM pairing, and access control management.

use clap::Subcommand;
use serde_json::json;

use crate::cli::ws;
use crate::error::{Result, SyscityError};

#[derive(Debug, Subcommand)]
pub enum SecurityCommands {
    /// Run comprehensive security audit
    Audit {
        /// Output format
        #[arg(short, long, value_enum, default_value = "table")]
        format: super::OutputFormat,
        /// Check specific paths for secrets
        #[arg(short, long)]
        paths: Vec<String>,
        /// Skip data leak checks
        #[arg(long)]
        skip_leaks: bool,
        /// Skip sandbox verification
        #[arg(long)]
        skip_sandbox: bool,
    },
    /// Show security status summary
    Status,
    /// Manage DM pairing (approve/reject pending requests)
    Pairing {
        #[command(subcommand)]
        command: PairingCommands,
    },
    /// List authorized users (approved or allowlisted)
    List {
        /// Channel type to filter by
        #[arg(short, long)]
        channel: Option<String>,
    },
    /// Revoke user access
    Revoke {
        /// Channel type
        #[arg(short, long)]
        channel: String,
        /// User ID to revoke
        #[arg(short, long)]
        user_id: String,
    },
    /// Manage command gate permission levels
    Gate {
        #[command(subcommand)]
        command: GateCommands,
    },
}

#[derive(Debug, Subcommand)]
pub enum GateCommands {
    /// Set a user's permission level
    Set {
        /// User ID
        user_id: String,
        /// Permission level: chat, user, admin
        level: String,
    },
    /// List all configured gate levels
    List,
    /// Clear a user's custom gate level
    Clear {
        /// User ID
        user_id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum PairingCommands {
    /// List pending pairing requests
    List {
        /// Channel to filter by
        #[arg(short, long)]
        channel: Option<String>,
    },
    /// Approve a pending pairing request by code
    Approve {
        /// Channel (e.g., telegram, discord)
        channel: String,
        /// Pairing code (e.g., ABC123)
        code: String,
        /// Admin name/ID (for audit trail)
        #[arg(short, long)]
        as_admin: Option<String>,
    },
    /// Reject/deny a pending pairing request
    Reject {
        /// Channel
        channel: String,
        /// Pairing code
        code: String,
    },
    /// Add user directly to allowlist (bypass pairing flow)
    Allow {
        /// Channel
        #[arg(short, long)]
        channel: String,
        /// User ID
        #[arg(short, long)]
        user_id: String,
        /// Optional username/handle
        #[arg(short, long)]
        username: Option<String>,
    },
}

/// Run security commands
pub async fn run_security_command(command: &SecurityCommands) -> Result<()> {
    match command {
        SecurityCommands::Pairing { command } => match command {
            PairingCommands::List { channel: _channel } => {
                let payload = ws::call("device.pairing.pending", json!({})).await?;
                let requests = payload.get("pending").and_then(|r| r.as_array());
                match requests {
                    Some(requests) if requests.is_empty() => {
                        println!("No pending pairing requests.")
                    }
                    Some(requests) => {
                        println!("Pending Pairing Requests:");
                        println!("{:<12} {:<20} {:<20} Created", "Code", "Device ID", "Name");
                        println!("{}", "-".repeat(70));
                        for req in requests {
                            println!(
                                "{:<12} {:<20} {:<20} {}",
                                req.get("code").and_then(|c| c.as_str()).unwrap_or("-"),
                                req.get("device_id").and_then(|c| c.as_str()).unwrap_or("-"),
                                req.get("display_name")
                                    .and_then(|c| c.as_str())
                                    .unwrap_or("-"),
                                req.get("created_at")
                                    .and_then(|c| c.as_str())
                                    .unwrap_or("-"),
                            );
                        }
                    }
                    None => println!("No pending pairing requests."),
                }
                Ok(())
            }
            PairingCommands::Approve {
                channel: _channel,
                code,
                as_admin: _as_admin,
            } => {
                match ws::call("device.pairing.approve", json!({ "code": code })).await {
                    Ok(_) => println!("✅ Approved pairing request {}", code),
                    Err(e) => {
                        eprintln!("Failed to approve: {}", e);
                        return Err(e);
                    }
                }
                Ok(())
            }
            PairingCommands::Reject { channel: _channel, code } => {
                match ws::call("device.pairing.reject", json!({ "code": code })).await {
                    Ok(_) => println!("❌ Rejected pairing request {}", code),
                    Err(e) => {
                        eprintln!("Failed to reject: {}", e);
                        return Err(e);
                    }
                }
                Ok(())
            }
            PairingCommands::Allow { channel, user_id, username } => {
                match ws::call(
                    "security.allowlist.add",
                    json!({ "channel": channel, "user_id": user_id, "username": username }),
                )
                .await
                {
                    Ok(_) => println!("✅ Added {} to allowlist for {}", user_id, channel),
                    Err(e) => {
                        eprintln!("Failed to add to allowlist: {}", e);
                        return Err(e);
                    }
                }
                Ok(())
            }
        },

        SecurityCommands::Audit {
            format,
            paths,
            skip_leaks,
            skip_sandbox,
        } => {
            // Run local security audit
            let _config = crate::config::Config::load()?;
            let mut audit_config = crate::security::audit::AuditConfig::default();

            if !paths.is_empty() {
                audit_config.paths_to_check = paths.clone();
            }
            audit_config.check_log_leaks = !skip_leaks;
            audit_config.verify_sandbox = !skip_sandbox;

            let auditor = crate::security::audit::SecurityAuditor::with_config(audit_config);
            let report = auditor.run_audit().await;

            // Output based on format
            match format {
                super::OutputFormat::Json => {
                    // The payload is built by the report itself so that
                    // `syscity audit security --format json` cannot drift from
                    // this one.
                    match serde_json::to_string_pretty(&report.to_json()) {
                        Ok(text) => println!("{text}"),
                        Err(e) => eprintln!("Failed to render JSON: {e}"),
                    }
                }
                super::OutputFormat::Yaml => {
                    println!("Security Audit Report");
                    println!("====================");
                    println!("Score: {}/100", report.score);
                    println!("Critical Issues: {}", report.critical_issues.len());
                    println!("Warnings: {}", report.warnings.len());
                    println!();

                    if !report.critical_issues.is_empty() {
                        println!("CRITICAL ISSUES:");
                        for issue in &report.critical_issues {
                            println!("  [!] {}", issue.description);
                            println!("      Location: {}", issue.location);
                            println!("      Fix: {}", issue.recommendation);
                            println!();
                        }
                    }

                    if !report.warnings.is_empty() {
                        println!("WARNINGS:");
                        for warning in &report.warnings {
                            println!("  [-] {}", warning.description);
                            println!("      Location: {}", warning.location);
                            println!();
                        }
                    }

                    if !report.recommendations.is_empty() {
                        println!("RECOMMENDATIONS:");
                        for rec in &report.recommendations {
                            println!("  * {}", rec);
                        }
                    }
                }
                _ => {
                    // Table / Plain format
                    println!("╔══════════════════════════════════════════════════════════════╗");
                    println!("║              SECURITY AUDIT REPORT                           ║");
                    println!("╠══════════════════════════════════════════════════════════════╣");

                    let score_color = if report.score >= 80 {
                        "🟢"
                    } else if report.score >= 60 {
                        "🟡"
                    } else if report.score >= 40 {
                        "🟠"
                    } else {
                        "🔴"
                    };

                    println!(
                        "║  Overall Score: {} {}/100                           ║",
                        score_color, report.score
                    );
                    println!("║                                                              ║");
                    println!(
                        "║  Permissions:  {}/{} passed                           ║",
                        report.permissions.passed, report.permissions.total_checks
                    );
                    println!(
                        "║  Tools:        {}/{} passing                          ║",
                        report.tools.passing, report.tools.total_tools
                    );
                    println!(
                        "║  Data Leaks:   {} found in {} checks                  ║",
                        report.data_leaks.leaks_found, report.data_leaks.checks_performed
                    );
                    println!(
                        "║  Sandbox:      {}                                    ║",
                        if report.sandbox.enabled {
                            "✅ Enabled"
                        } else {
                            "❌ Disabled"
                        }
                    );
                    println!("╚══════════════════════════════════════════════════════════════╝");
                    println!();

                    if !report.critical_issues.is_empty() {
                        println!("🔴 CRITICAL ISSUES ({}):", report.critical_issues.len());
                        for (i, issue) in report.critical_issues.iter().enumerate() {
                            println!("  {}. [{}] {}", i + 1, issue.category, issue.description);
                            println!("     Location: {}", issue.location);
                            println!("     Recommendation: {}", issue.recommendation);
                            println!();
                        }
                    }

                    if !report.warnings.is_empty() {
                        println!("🟡 WARNINGS ({}):", report.warnings.len());
                        for (i, warning) in report.warnings.iter().enumerate() {
                            println!("  {}. [{}] {}", i + 1, warning.category, warning.description);
                        }
                        println!();
                    }

                    if !report.recommendations.is_empty() {
                        println!("💡 RECOMMENDATIONS:");
                        for rec in &report.recommendations {
                            println!("  • {}", rec);
                        }
                    }

                    if report.critical_issues.is_empty() && report.warnings.is_empty() {
                        println!("✅ No critical issues or warnings found!");
                    }
                }
            }

            // Return error if critical issues found (for CI/CD use)
            if !report.critical_issues.is_empty() {
                return Err(SyscityError::Validation(format!(
                    "Security audit found {} critical issues",
                    report.critical_issues.len()
                )));
            }

            Ok(())
        }

        SecurityCommands::Status => {
            let payload = ws::call("security.status", json!({})).await?;
            println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
            Ok(())
        }

        SecurityCommands::List { channel: _channel } => {
            let payload = ws::call("device.pairing.authorized", json!({})).await?;
            println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
            Ok(())
        }

        SecurityCommands::Revoke { channel: _channel, user_id } => {
            match ws::call("device.pairing.revoke", json!({ "device_id": user_id })).await {
                Ok(_) => println!("✅ Revoked access for {}", user_id),
                Err(e) => {
                    eprintln!("Failed to revoke: {}", e);
                    return Err(e);
                }
            }
            Ok(())
        }

        SecurityCommands::Gate { command } => match command {
            GateCommands::Set { user_id, level } => {
                match ws::call("security.gate.set", json!({ "user_id": user_id, "level": level }))
                    .await
                {
                    Ok(_) => println!("✅ Set gate level for {} to {}", user_id, level),
                    Err(e) => {
                        eprintln!("Failed to set gate level: {}", e);
                        return Err(e);
                    }
                }
                Ok(())
            }
            GateCommands::List => {
                let body = ws::call("security.gate.list", json!({})).await?;
                if let Some(levels) = body.get("levels").and_then(|l| l.as_object()) {
                    if levels.is_empty() {
                        println!("No custom gate levels configured.");
                    } else {
                        println!("{:<20} Level", "User ID");
                        println!("{}", "-".repeat(40));
                        for (user, level) in levels {
                            println!("{:<20} {}", user, level.as_str().unwrap_or("?"));
                        }
                    }
                }
                println!("\nDefault level: user");
                Ok(())
            }
            GateCommands::Clear { user_id } => {
                match ws::call("security.gate.clear", json!({ "user_id": user_id })).await {
                    Ok(_) => println!("✅ Cleared gate level for {}", user_id),
                    Err(e) => {
                        eprintln!("Failed to clear gate level: {}", e);
                        return Err(e);
                    }
                }
                Ok(())
            }
        },
    }
}
