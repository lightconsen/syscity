//! Audit log commands for Syscity
//!
//! View system audit logs and security audit results.

use clap::Subcommand;

use crate::cli::ws;
use crate::error::Result;
use serde_json::json;

#[derive(Debug, Subcommand)]
pub enum AuditCommands {
    /// View the system audit log
    Log {
        /// Number of entries to show (default: 50)
        #[arg(short = 'n', long, default_value = "50")]
        limit: usize,
        /// Filter by event type
        #[arg(short, long)]
        event_type: Option<String>,
    },
    /// Run a local security audit
    Security {
        /// Output format
        #[arg(short, long, value_enum, default_value = "table")]
        format: super::OutputFormat,
    },
}

/// Run audit commands
pub async fn run_audit_command(command: &AuditCommands) -> Result<()> {
    match command {
        AuditCommands::Log { limit, event_type } => {
            let body =
                ws::call("audit.recent", json!({ "limit": limit, "event_type": event_type }))
                    .await?;
            let entries = body.get("entries").and_then(|e| e.as_array());
            match entries {
                Some(entries) if entries.is_empty() => println!("No audit log entries."),
                Some(entries) => {
                    println!("Audit Log:");
                    println!("{:<20} {:<15} {:<20} Details", "Timestamp", "Event", "User");
                    println!("{}", "-".repeat(90));
                    for entry in entries {
                        println!(
                            "{:<20} {:<15} {:<20} {}",
                            entry
                                .get("timestamp")
                                .and_then(|c| c.as_str())
                                .unwrap_or("-"),
                            entry
                                .get("event_type")
                                .and_then(|c| c.as_str())
                                .unwrap_or("-"),
                            entry.get("user_id").and_then(|c| c.as_str()).unwrap_or("-"),
                            entry
                                .get("details")
                                .and_then(|c| c.as_str())
                                .unwrap_or("-")
                                .chars()
                                .take(40)
                                .collect::<String>(),
                        );
                    }
                }
                None => println!("No audit log entries."),
            }
            Ok(())
        }
        AuditCommands::Security { format } => {
            // Run local security audit (same as `syscity security audit`)
            let _config = crate::config::Config::load()?;
            let auditor = crate::security::audit::SecurityAuditor::with_config(
                crate::security::audit::AuditConfig::default(),
            );
            let result = auditor.run_audit().await;

            print!("{}", render_security_audit(&result, *format));
            Ok(())
        }
    }
}

/// Render an audit report in the requested format.
///
/// `--format json` and `--format yaml` used to print the human table — the
/// same branch body as the table arm — so a caller asking for machine-readable
/// output got neither, plus no issue lists. Both are real now, from the same
/// payload `syscity security audit --format json` emits.
fn render_security_audit(
    report: &crate::security::audit::SecurityAuditReport,
    format: super::OutputFormat,
) -> String {
    use std::fmt::Write as _;

    match format {
        super::OutputFormat::Json => match serde_json::to_string_pretty(&report.to_json()) {
            Ok(text) => format!("{text}\n"),
            Err(e) => format!("{{\"error\": \"failed to render report: {e}\"}}\n"),
        },
        super::OutputFormat::Yaml => match serde_norway::to_string(&report.to_json()) {
            Ok(text) => text,
            Err(e) => format!("# failed to render report as YAML: {e}\n"),
        },
        super::OutputFormat::Table | super::OutputFormat::Plain => {
            let mut out = String::new();
            let _ = writeln!(out, "Security Audit Results");
            let _ = writeln!(out, "======================");
            let _ = writeln!(out, "Score: {}/100", report.score);
            let _ = writeln!(out, "Timestamp: {:?}", report.timestamp);
            let _ = writeln!(
                out,
                "Permissions: {}/{} passed",
                report.permissions.passed, report.permissions.total_checks
            );
            let _ = writeln!(
                out,
                "Tools: {}/{} passing",
                report.tools.passing, report.tools.total_tools
            );
            let _ = writeln!(
                out,
                "Data Leaks: {} found in {} checks",
                report.data_leaks.leaks_found, report.data_leaks.checks_performed
            );
            if !report.critical_issues.is_empty() {
                let _ = writeln!(out, "\n❌ Critical Issues:");
                for issue in &report.critical_issues {
                    let _ = writeln!(out, "  - {}", issue.description);
                }
            }
            if !report.warnings.is_empty() {
                let _ = writeln!(out, "\n⚠️  Warnings:");
                for warning in &report.warnings {
                    let _ = writeln!(out, "  - {}", warning.description);
                }
            }
            if !report.recommendations.is_empty() {
                let _ = writeln!(out, "\n💡 Recommendations:");
                for rec in &report.recommendations {
                    let _ = writeln!(out, "  - {}", rec);
                }
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::audit::{
        DataLeakAudit, PermissionAudit, RiskLevel, SandboxAudit, SecurityAuditReport,
        SecurityBoundaries, SecurityIssue, ToolAudit,
    };
    use std::time::SystemTime;

    fn report() -> SecurityAuditReport {
        SecurityAuditReport {
            timestamp: SystemTime::UNIX_EPOCH,
            score: 72,
            permissions: PermissionAudit {
                total_checks: 5,
                passed: 4,
                failed: 1,
                ..Default::default()
            },
            tools: ToolAudit::default(),
            data_leaks: DataLeakAudit::default(),
            sandbox: SandboxAudit::default(),
            boundaries: SecurityBoundaries::default(),
            critical_issues: vec![SecurityIssue {
                category: "files".into(),
                severity: RiskLevel::High,
                description: "world-writable config".into(),
                location: "~/.syscity/config.toml".into(),
                recommendation: "chmod 600".into(),
            }],
            warnings: vec![],
            recommendations: vec!["rotate the shared token".into()],
        }
    }

    /// `--format json` must be JSON. It used to print the human table, so a
    /// caller piping it into `jq` got a syntax error.
    #[test]
    fn json_format_is_json_with_the_issues_in_it() {
        let out = render_security_audit(&report(), crate::cli::OutputFormat::Json);
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("--format json must parse as JSON");
        assert_eq!(parsed["score"], 72);
        assert_eq!(parsed["critical_issues"], 1);
        assert_eq!(parsed["permissions"]["failed"], 1);
        // The point of asking for JSON: the findings, not just their counts.
        assert_eq!(parsed["critical_issues_list"][0]["location"], "~/.syscity/config.toml");
        assert_eq!(parsed["critical_issues_list"][0]["severity"], "High");
        assert_eq!(parsed["recommendations"][0], "rotate the shared token");
    }

    /// `--format yaml` must be YAML, with the same content.
    #[test]
    fn yaml_format_is_yaml() {
        let out = render_security_audit(&report(), crate::cli::OutputFormat::Yaml);
        let parsed: serde_json::Value =
            serde_norway::from_str(&out).expect("--format yaml must parse as YAML");
        assert_eq!(parsed["score"], 72);
        assert_eq!(parsed["critical_issues_list"][0]["category"], "files");
    }

    /// The default stays the human table, and the two machine formats stay
    /// distinguishable from it.
    #[test]
    fn table_format_stays_human_readable() {
        let out = render_security_audit(&report(), crate::cli::OutputFormat::Table);
        assert!(out.contains("Score: 72/100"));
        assert!(out.contains("world-writable config"));
        assert!(!out.trim_start().starts_with('{'), "the table is not JSON");
        assert!(serde_json::from_str::<serde_json::Value>(&out).is_err());
    }
}
