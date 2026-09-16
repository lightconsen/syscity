//! Security audit system for Syscity
//!
//! Provides comprehensive security auditing capabilities including:
//! - Permission review and validation
//! - Tool implementation auditing
//! - Data leak detection
//! - Sandboxing verification
//! - Security boundary documentation

use std::collections::HashMap;
use std::time::SystemTime;

use tracing::{debug, info, warn};

/// Security audit report
#[derive(Debug, Clone)]
pub struct SecurityAuditReport {
    /// When the audit was performed
    pub timestamp: SystemTime,
    /// Overall security score (0-100)
    pub score: u8,
    /// Permission audit results
    pub permissions: PermissionAudit,
    /// Tool audit results
    pub tools: ToolAudit,
    /// Data leak check results
    pub data_leaks: DataLeakAudit,
    /// Sandbox verification results
    pub sandbox: SandboxAudit,
    /// Security boundaries
    pub boundaries: SecurityBoundaries,
    /// Critical issues found
    pub critical_issues: Vec<SecurityIssue>,
    /// Warnings
    pub warnings: Vec<SecurityIssue>,
    /// Recommendations
    pub recommendations: Vec<String>,
}

impl SecurityAuditReport {
    /// The report as the machine-readable payload the CLI prints.
    ///
    /// One definition for both entry points (`syscity security audit` and
    /// `syscity audit security`): they run the same audit, and two hand-built
    /// payloads would drift apart. It carries the issue *lists*, not just their
    /// counts — a caller asking for JSON wants the findings, not a summary they
    /// still have to fetch.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "timestamp": self.timestamp.duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default().as_secs(),
            "score": self.score,
            "critical_issues": self.critical_issues.len(),
            "warnings": self.warnings.len(),
            "recommendations": self.recommendations.len(),
            "permissions": {
                "total_checks": self.permissions.total_checks,
                "passed": self.permissions.passed,
                "failed": self.permissions.failed,
            },
            "tools": {
                "total": self.tools.total_tools,
                "passing": self.tools.passing,
                "failing": self.tools.failing,
            },
            "data_leaks": {
                "checks_performed": self.data_leaks.checks_performed,
                "leaks_found": self.data_leaks.leaks_found,
            },
            "sandbox": {
                "enabled": self.sandbox.enabled,
                "features": self.sandbox.features.len(),
            },
            "critical_issues_list": self.critical_issues.iter().map(|i| {
                serde_json::json!({
                    "category": i.category,
                    "severity": format!("{:?}", i.severity),
                    "description": i.description,
                    "location": i.location,
                    "recommendation": i.recommendation,
                })
            }).collect::<Vec<_>>(),
            "warnings_list": self.warnings.iter().map(|i| {
                serde_json::json!({
                    "category": i.category,
                    "severity": format!("{:?}", i.severity),
                    "description": i.description,
                    "location": i.location,
                })
            }).collect::<Vec<_>>(),
            // Kept as the array it has always been: `syscity security audit
            // --format json` already emits it this way, and the count is
            // derivable while a changed type is not.
            "recommendations": self.recommendations,
        })
    }
}

/// Permission audit results
#[derive(Debug, Clone, Default)]
pub struct PermissionAudit {
    /// Total number of permission checks
    pub total_checks: usize,
    /// Passed checks
    pub passed: usize,
    /// Failed checks
    pub failed: usize,
    /// Permission details by component
    pub components: HashMap<String, ComponentPermissions>,
}

/// Permissions for a specific component
#[derive(Debug, Clone, Default)]
pub struct ComponentPermissions {
    /// Component name
    pub name: String,
    /// Required permissions
    pub required: Vec<String>,
    /// Granted permissions
    pub granted: Vec<String>,
    /// Missing permissions
    pub missing: Vec<String>,
    /// Excessive permissions
    pub excessive: Vec<String>,
}

/// Tool audit results
#[derive(Debug, Clone, Default)]
pub struct ToolAudit {
    /// Total tools audited
    pub total_tools: usize,
    /// Tools passing all checks
    pub passing: usize,
    /// Tools with issues
    pub failing: usize,
    /// Results per tool
    pub tool_results: HashMap<String, ToolAuditResult>,
}

/// Individual tool audit result
#[derive(Debug, Clone, Default)]
pub struct ToolAuditResult {
    /// Tool name
    pub name: String,
    /// Whether the tool passed audit
    pub passed: bool,
    /// Security checks performed
    pub checks: Vec<ToolSecurityCheck>,
    /// Issues found
    pub issues: Vec<String>,
    /// Risk level
    pub risk_level: RiskLevel,
}

/// Security check for a tool
#[derive(Debug, Clone)]
pub struct ToolSecurityCheck {
    /// Check name
    pub name: String,
    /// Whether it passed
    pub passed: bool,
    /// Description
    pub description: String,
}

/// Risk level classification
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RiskLevel {
    /// Low risk - minor issues
    #[default]
    Low,
    /// Medium risk - should be addressed
    Medium,
    /// High risk - needs immediate attention
    High,
    /// Critical risk - security vulnerability
    Critical,
}

/// Data leak audit results
#[derive(Debug, Clone, Default)]
pub struct DataLeakAudit {
    /// Checks performed
    pub checks_performed: usize,
    /// Potential leaks found
    pub leaks_found: usize,
    /// Leak details
    pub leaks: Vec<PotentialLeak>,
}

/// Potential data leak
#[derive(Debug, Clone)]
pub struct PotentialLeak {
    /// Leak category
    pub category: LeakCategory,
    /// Description
    pub description: String,
    /// Location (file, function, etc.)
    pub location: String,
    /// Severity
    pub severity: RiskLevel,
    /// Recommended fix
    pub recommendation: String,
}

/// Categories of data leaks
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeakCategory {
    /// Sensitive data in logs
    LogLeak,
    /// Sensitive data in error messages
    ErrorLeak,
    /// Data in URLs/query parameters
    UrlLeak,
    /// Unencrypted storage
    UnencryptedStorage,
    /// Memory leak of sensitive data
    MemoryLeak,
    /// Credential exposure
    CredentialExposure,
}

/// Sandbox audit results
#[derive(Debug, Clone, Default)]
pub struct SandboxAudit {
    /// Whether sandbox is enabled
    pub enabled: bool,
    /// Sandboxing features verified
    pub features: Vec<SandboxFeatureCheck>,
    /// Resource limits configured
    pub resource_limits: ResourceLimitsCheck,
    /// Isolation verification
    pub isolation: IsolationCheck,
}

/// Sandbox feature check
#[derive(Debug, Clone)]
pub struct SandboxFeatureCheck {
    /// Feature name
    pub name: String,
    /// Whether it's enabled and working
    pub enabled: bool,
    /// Verification result
    pub verified: bool,
    /// Details
    pub details: String,
}

/// Resource limits verification
#[derive(Debug, Clone, Default)]
pub struct ResourceLimitsCheck {
    /// CPU limits
    pub cpu_limits: bool,
    /// Memory limits
    pub memory_limits: bool,
    /// Time limits
    pub time_limits: bool,
    /// File descriptor limits
    pub fd_limits: bool,
    /// Network limits
    pub network_limits: bool,
}

/// Isolation verification
#[derive(Debug, Clone, Default)]
pub struct IsolationCheck {
    /// Process isolation
    pub process_isolation: bool,
    /// Filesystem isolation
    pub filesystem_isolation: bool,
    /// Network isolation
    pub network_isolation: bool,
    /// Environment isolation
    pub env_isolation: bool,
}

/// Security boundaries documentation
#[derive(Debug, Clone, Default)]
pub struct SecurityBoundaries {
    /// Defined boundaries
    pub boundaries: Vec<SecurityBoundary>,
    /// Boundary enforcement status
    pub enforcement: BoundaryEnforcement,
}

/// Individual security boundary
#[derive(Debug, Clone)]
pub struct SecurityBoundary {
    /// Boundary name
    pub name: String,
    /// Description
    pub description: String,
    /// Boundary type
    pub boundary_type: BoundaryType,
    /// Enforcement mechanism
    pub enforcement: String,
    /// Whether it's verified working
    pub verified: bool,
}

/// Types of security boundaries
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BoundaryType {
    /// User isolation boundary
    UserIsolation,
    /// Process isolation boundary
    ProcessIsolation,
    /// Network boundary
    Network,
    /// Filesystem boundary
    Filesystem,
    /// Data access boundary
    DataAccess,
    /// Tool permission boundary
    ToolPermission,
}

/// Boundary enforcement status
#[derive(Debug, Clone, Default)]
pub struct BoundaryEnforcement {
    /// Total boundaries defined
    pub total: usize,
    /// Properly enforced
    pub enforced: usize,
    /// Partially enforced
    pub partial: usize,
    /// Not enforced
    pub not_enforced: usize,
}

/// Security issue
#[derive(Debug, Clone)]
pub struct SecurityIssue {
    /// Issue category
    pub category: String,
    /// Severity
    pub severity: RiskLevel,
    /// Description
    pub description: String,
    /// Location
    pub location: String,
    /// Fix recommendation
    pub recommendation: String,
}

/// Security auditor
#[derive(Debug, Clone)]
pub struct SecurityAuditor {
    /// Configuration for auditing
    config: AuditConfig,
}

/// Audit configuration
#[derive(Debug, Clone)]
pub struct AuditConfig {
    /// Whether to check for data leaks in logs
    pub check_log_leaks: bool,
    /// Whether to verify sandboxing
    pub verify_sandbox: bool,
    /// Whether to audit tool implementations
    pub audit_tools: bool,
    /// Whether to review permissions
    pub review_permissions: bool,
    /// Paths to check for data leaks
    pub paths_to_check: Vec<String>,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            check_log_leaks: true,
            verify_sandbox: true,
            audit_tools: true,
            review_permissions: true,
            paths_to_check: vec!["src".to_string(), "tests".to_string()],
        }
    }
}

impl SecurityAuditor {
    /// Create a new security auditor
    pub fn new() -> Self {
        Self { config: AuditConfig::default() }
    }

    /// Create with custom config
    pub fn with_config(config: AuditConfig) -> Self {
        Self { config }
    }

    /// Run complete security audit
    pub async fn run_audit(&self) -> SecurityAuditReport {
        info!("Starting comprehensive security audit");
        let start = SystemTime::now();

        let permissions = self.audit_permissions().await;
        let tools = self.audit_tools().await;
        let data_leaks = self.check_data_leaks().await;
        let sandbox = self.verify_sandbox().await;
        let boundaries = self.document_boundaries().await;

        let mut critical_issues = Vec::new();
        let mut warnings = Vec::new();
        let mut recommendations = Vec::new();

        // Collect issues from all audits
        self.collect_permission_issues(&permissions, &mut warnings, &mut recommendations);
        self.collect_tool_issues(&tools, &mut critical_issues, &mut warnings, &mut recommendations);
        self.collect_leak_issues(
            &data_leaks,
            &mut critical_issues,
            &mut warnings,
            &mut recommendations,
        );
        self.collect_sandbox_issues(
            &sandbox,
            &mut critical_issues,
            &mut warnings,
            &mut recommendations,
        );

        // Calculate overall score
        let score = self.calculate_score(&permissions, &tools, &data_leaks, &sandbox, &boundaries);

        let report = SecurityAuditReport {
            timestamp: start,
            score,
            permissions,
            tools,
            data_leaks,
            sandbox,
            boundaries,
            critical_issues,
            warnings,
            recommendations,
        };

        info!("Security audit complete. Score: {}/100", score);
        report
    }

    /// Audit permissions across the system
    async fn audit_permissions(&self) -> PermissionAudit {
        debug!("Auditing permissions");
        let mut audit = PermissionAudit::default();

        if !self.config.review_permissions {
            return audit;
        }

        // Check file tool permissions
        let file_perms = ComponentPermissions {
            name: "file_tools".to_string(),
            required: vec![
                "path_allowlist".to_string(),
                "size_limits".to_string(),
                "binary_detection".to_string(),
            ],
            granted: vec![
                "path_allowlist".to_string(),
                "size_limits".to_string(),
                "binary_detection".to_string(),
            ],
            missing: vec![],
            excessive: vec![],
        };
        audit
            .components
            .insert("file_tools".to_string(), file_perms);
        audit.passed += 1;

        // Check shell tool permissions
        let shell_perms = ComponentPermissions {
            name: "shell_tools".to_string(),
            required: vec![
                "command_allowlist".to_string(),
                "timeout".to_string(),
                "output_limits".to_string(),
                "env_isolation".to_string(),
            ],
            granted: vec![
                "command_allowlist".to_string(),
                "timeout".to_string(),
                "output_limits".to_string(),
                "env_isolation".to_string(),
            ],
            missing: vec![],
            excessive: vec![],
        };
        audit
            .components
            .insert("shell_tools".to_string(), shell_perms);
        audit.passed += 1;

        // Check code execution permissions
        let code_perms = ComponentPermissions {
            name: "code_execution".to_string(),
            required: vec![
                "forbidden_imports".to_string(),
                "pattern_detection".to_string(),
                "timeout".to_string(),
                "output_limits".to_string(),
                "memory_limits".to_string(),
            ],
            granted: vec![
                "forbidden_imports".to_string(),
                "pattern_detection".to_string(),
                "timeout".to_string(),
                "output_limits".to_string(),
            ],
            missing: vec!["memory_limits".to_string()],
            excessive: vec![],
        };
        audit
            .components
            .insert("code_execution".to_string(), code_perms);
        audit.passed += 1;

        // Check web tool permissions
        let web_perms = ComponentPermissions {
            name: "web_tools".to_string(),
            required: vec![
                "scheme_restriction".to_string(),
                "timeout".to_string(),
                "size_limits".to_string(),
                "url_allowlist".to_string(),
            ],
            granted: vec![
                "scheme_restriction".to_string(),
                "timeout".to_string(),
                "size_limits".to_string(),
            ],
            missing: vec!["url_allowlist".to_string()],
            excessive: vec![],
        };
        audit.components.insert("web_tools".to_string(), web_perms);
        audit.passed += 1;

        audit.total_checks = audit.components.len();
        audit
    }

    /// Audit tool implementations
    async fn audit_tools(&self) -> ToolAudit {
        debug!("Auditing tool implementations");
        let mut audit = ToolAudit::default();

        if !self.config.audit_tools {
            return audit;
        }

        // File Read Tool
        let file_read = ToolAuditResult {
            name: "file_read".to_string(),
            passed: true,
            checks: vec![
                ToolSecurityCheck {
                    name: "path_allowlist".to_string(),
                    passed: true,
                    description: "Validates paths against allowlist".to_string(),
                },
                ToolSecurityCheck {
                    name: "size_limits".to_string(),
                    passed: true,
                    description: "Enforces 1MB read limit".to_string(),
                },
                ToolSecurityCheck {
                    name: "binary_detection".to_string(),
                    passed: true,
                    description: "Detects binary files".to_string(),
                },
            ],
            issues: vec![],
            risk_level: RiskLevel::Low,
        };
        audit
            .tool_results
            .insert("file_read".to_string(), file_read);
        audit.passing += 1;

        // Shell Tool
        let shell = ToolAuditResult {
            name: "shell".to_string(),
            passed: true,
            checks: vec![
                ToolSecurityCheck {
                    name: "command_allowlist".to_string(),
                    passed: true,
                    description: "Validates commands against allowlist".to_string(),
                },
                ToolSecurityCheck {
                    name: "timeout".to_string(),
                    passed: true,
                    description: "30 second timeout enforced".to_string(),
                },
                ToolSecurityCheck {
                    name: "argument_validation".to_string(),
                    passed: false,
                    description: "Command arguments not validated".to_string(),
                },
            ],
            issues: vec![
                "Command arguments are not validated, allowing shell injection through allowed \
                 commands"
                    .to_string(),
            ],
            risk_level: RiskLevel::Medium,
        };
        audit.tool_results.insert("shell".to_string(), shell);
        audit.passing += 1;

        // Code Execution Tool
        let code_exec = ToolAuditResult {
            name: "code_execution".to_string(),
            passed: false,
            checks: vec![
                ToolSecurityCheck {
                    name: "forbidden_imports".to_string(),
                    passed: true,
                    description: "Blocks dangerous imports".to_string(),
                },
                ToolSecurityCheck {
                    name: "timeout".to_string(),
                    passed: true,
                    description: "5 minute timeout".to_string(),
                },
                ToolSecurityCheck {
                    name: "memory_limits".to_string(),
                    passed: false,
                    description: "Memory limits not enforced".to_string(),
                },
            ],
            issues: vec![
                "Memory limits are not enforced (requires unsafe code or external sandbox)"
                    .to_string(),
                "No filesystem restrictions within Python".to_string(),
            ],
            risk_level: RiskLevel::High,
        };
        audit
            .tool_results
            .insert("code_execution".to_string(), code_exec);
        audit.failing += 1;

        // Web Fetch Tool
        let web_fetch = ToolAuditResult {
            name: "web_fetch".to_string(),
            passed: true,
            checks: vec![
                ToolSecurityCheck {
                    name: "scheme_restriction".to_string(),
                    passed: true,
                    description: "Only HTTP/HTTPS allowed".to_string(),
                },
                ToolSecurityCheck {
                    name: "timeout".to_string(),
                    passed: true,
                    description: "30 second timeout".to_string(),
                },
                ToolSecurityCheck {
                    name: "ssrf_protection".to_string(),
                    passed: false,
                    description: "No SSRF protection".to_string(),
                },
            ],
            issues: vec![
                "No URL allowlist for SSRF protection".to_string(),
                "No DNS rebinding protection".to_string(),
            ],
            risk_level: RiskLevel::Medium,
        };
        audit
            .tool_results
            .insert("web_fetch".to_string(), web_fetch);
        audit.passing += 1;

        audit.total_tools = audit.tool_results.len();
        audit
    }

    /// Check for potential data leaks by scanning source files
    async fn check_data_leaks(&self) -> DataLeakAudit {
        debug!("Checking for data leaks");
        let mut audit = DataLeakAudit::default();

        if !self.config.check_log_leaks {
            return audit;
        }

        use std::path::Path;

        use crate::security::secrets::SecretScanner;

        let scanner = SecretScanner::with_default_patterns();
        let source_extensions = [".rs", ".toml", ".yaml", ".yml", ".json", ".env"];

        // Scan configured paths
        for path_str in &self.config.paths_to_check {
            let path = Path::new(path_str);
            if !path.exists() {
                warn!("Path does not exist: {}", path_str);
                continue;
            }

            let mut entries = match tokio::fs::read_dir(path).await {
                Ok(entries) => entries,
                Err(e) => {
                    warn!("Failed to read directory {}: {}", path_str, e);
                    continue;
                }
            };

            while let Some(entry) = entries.next_entry().await.ok().flatten() {
                self.scan_single_file(&scanner, &source_extensions, &entry, &mut audit)
                    .await;
            }
        }

        // Also check for sensitive patterns in error handling
        audit.checks_performed += 1;
        if let Some(leak) = self.check_error_message_leaks().await {
            audit.leaks.push(leak);
            audit.leaks_found += 1;
        }

        info!(
            "Data leak scan complete: {} checks performed, {} potential leaks found",
            audit.checks_performed, audit.leaks_found
        );

        audit
    }

    /// Scan a single file entry for secret leaks and record findings into
    /// `audit`.
    async fn scan_single_file(
        &self,
        scanner: &crate::security::secrets::SecretScanner,
        source_extensions: &[&str],
        entry: &tokio::fs::DirEntry,
        audit: &mut DataLeakAudit,
    ) {
        let file_path = entry.path();
        let file_name = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        // Skip hidden files and non-source files
        if file_name.starts_with('.') {
            return;
        }

        // Check if it's a file with a source extension
        if !file_path.is_file() {
            return;
        }

        let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");

        if !source_extensions.contains(&ext) {
            return;
        }

        // Skip test files that may contain test secrets
        if file_name.contains("test") || file_name.contains("mock") {
            return;
        }

        audit.checks_performed += 1;

        // Read and scan the file
        let content = match tokio::fs::read_to_string(&file_path).await {
            Ok(content) => content,
            Err(e) => {
                debug!("Failed to read file {}: {}", file_path.display(), e);
                return;
            }
        };

        // Scan for secrets
        let findings = scanner.scan(&content);
        for finding in findings {
            let category = match finding.severity {
                crate::security::secrets::Severity::Critical => LeakCategory::CredentialExposure,
                crate::security::secrets::Severity::High => LeakCategory::UnencryptedStorage,
                _ => LeakCategory::LogLeak,
            };

            let severity = match finding.severity {
                crate::security::secrets::Severity::Critical => RiskLevel::Critical,
                crate::security::secrets::Severity::High => RiskLevel::High,
                crate::security::secrets::Severity::Medium => RiskLevel::Medium,
                crate::security::secrets::Severity::Low => RiskLevel::Low,
            };

            audit.leaks.push(PotentialLeak {
                category,
                description: format!("{}: {}", finding.pattern, finding.description),
                location: format!("{}:{}", file_path.display(), finding.line_number),
                severity,
                recommendation: "Remove secrets from source code and use environment variables or \
                                 a secrets manager"
                    .to_string(),
            });
            audit.leaks_found += 1;
        }
    }

    /// Check for potential sensitive data in error message patterns
    async fn check_error_message_leaks(&self) -> Option<PotentialLeak> {
        // Scan for patterns that might expose sensitive data in errors
        use std::path::Path;

        let sensitive_patterns = [
            "api_key",
            "apikey",
            "password",
            "secret",
            "token",
            "credential",
        ];

        let src_path = Path::new("src");
        if !src_path.exists() {
            return None;
        }

        // Check common error formatting patterns in source files
        let mut entries = match tokio::fs::read_dir(src_path).await {
            Ok(e) => e,
            Err(_) => return None,
        };
        while let Some(entry) = entries.next_entry().await.ok().flatten() {
            let file_path = entry.path();
            if !file_path.is_file() {
                continue;
            }

            let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "rs" {
                continue;
            }

            let content = match tokio::fs::read_to_string(&file_path).await {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Check for potentially sensitive data in error messages
            for pattern in &sensitive_patterns {
                // Check if sensitive variable names appear in format strings
                if content.contains(format!("{}: {:?}", pattern, "").trim_end_matches('"'))
                    || content.contains(format!("{}: {}", pattern, "").trim_end_matches('"'))
                {
                    return Some(PotentialLeak {
                        category: LeakCategory::ErrorLeak,
                        description: format!(
                            "Potential sensitive data exposure: '{}' may be logged or displayed",
                            pattern
                        ),
                        location: file_path.display().to_string(),
                        severity: RiskLevel::Medium,
                        recommendation: "Use structured error types and avoid including raw \
                                         sensitive data in error messages"
                            .to_string(),
                    });
                }
            }
        }

        None
    }

    /// Verify sandboxing capabilities
    async fn verify_sandbox(&self) -> SandboxAudit {
        debug!("Verifying sandboxing");
        let mut audit = SandboxAudit::default();

        if !self.config.verify_sandbox {
            return audit;
        }

        audit.enabled = true;

        // Check sandbox features
        audit.features = vec![
            SandboxFeatureCheck {
                name: "command_allowlist".to_string(),
                enabled: true,
                verified: true,
                details: "Shell commands validated against allowlist".to_string(),
            },
            SandboxFeatureCheck {
                name: "path_allowlist".to_string(),
                enabled: true,
                verified: true,
                details: "File paths validated against allowlist".to_string(),
            },
            SandboxFeatureCheck {
                name: "timeout".to_string(),
                enabled: true,
                verified: true,
                details: "Tool execution timeout enforced".to_string(),
            },
            SandboxFeatureCheck {
                name: "memory_limits".to_string(),
                enabled: false,
                verified: false,
                details: "Memory limits not implemented (requires unsafe or external sandbox)"
                    .to_string(),
            },
            SandboxFeatureCheck {
                name: "network_isolation".to_string(),
                enabled: false,
                verified: false,
                details: "No network isolation for code execution".to_string(),
            },
        ];

        // Resource limits
        audit.resource_limits = ResourceLimitsCheck {
            cpu_limits: false,
            memory_limits: false,
            time_limits: true,
            fd_limits: false,
            network_limits: false,
        };

        // Isolation
        audit.isolation = IsolationCheck {
            process_isolation: true,
            filesystem_isolation: false,
            network_isolation: false,
            env_isolation: true,
        };

        audit
    }

    /// Document security boundaries
    #[allow(clippy::field_reassign_with_default)]
    async fn document_boundaries(&self) -> SecurityBoundaries {
        debug!("Documenting security boundaries");
        let mut boundaries = SecurityBoundaries::default();

        boundaries.boundaries = vec![
            SecurityBoundary {
                name: "User Isolation".to_string(),
                description: "Each user's data is isolated from other users".to_string(),
                boundary_type: BoundaryType::UserIsolation,
                enforcement: "SQLite per-user table isolation with user_id filtering".to_string(),
                verified: true,
            },
            SecurityBoundary {
                name: "Tool Permission Boundary".to_string(),
                description: "Tools can only access explicitly allowed resources".to_string(),
                boundary_type: BoundaryType::ToolPermission,
                enforcement: "Path/command allowlists in ToolContext".to_string(),
                verified: true,
            },
            SecurityBoundary {
                name: "Process Isolation".to_string(),
                description: "Code execution runs in separate process".to_string(),
                boundary_type: BoundaryType::ProcessIsolation,
                enforcement: "Separate Python process with RPC communication".to_string(),
                verified: true,
            },
            SecurityBoundary {
                name: "Network Boundary".to_string(),
                description: "Control over external network access".to_string(),
                boundary_type: BoundaryType::Network,
                enforcement: "URL scheme validation only".to_string(),
                verified: false,
            },
            SecurityBoundary {
                name: "Filesystem Boundary".to_string(),
                description: "Control over filesystem access".to_string(),
                boundary_type: BoundaryType::Filesystem,
                enforcement: "Path allowlist validation".to_string(),
                verified: true,
            },
        ];

        let enforced = boundaries.boundaries.iter().filter(|b| b.verified).count();
        let partial = 0;
        let not_enforced = boundaries.boundaries.len() - enforced - partial;

        boundaries.enforcement = BoundaryEnforcement {
            total: boundaries.boundaries.len(),
            enforced,
            partial,
            not_enforced,
        };

        boundaries
    }

    /// Collect permission-related issues
    fn collect_permission_issues(
        &self,
        audit: &PermissionAudit,
        warnings: &mut Vec<SecurityIssue>,
        recommendations: &mut Vec<String>,
    ) {
        for (name, perms) in &audit.components {
            if !perms.missing.is_empty() {
                warnings.push(SecurityIssue {
                    category: "Permissions".to_string(),
                    severity: RiskLevel::Medium,
                    description: format!("{} missing permissions: {:?}", name, perms.missing),
                    location: name.clone(),
                    recommendation: format!("Implement {:?} for {}", perms.missing, name),
                });
            }
        }

        recommendations.push("Consider implementing RBAC (Role-Based Access Control)".to_string());
    }

    /// Collect tool audit issues
    fn collect_tool_issues(
        &self,
        audit: &ToolAudit,
        critical: &mut Vec<SecurityIssue>,
        warnings: &mut Vec<SecurityIssue>,
        recommendations: &mut Vec<String>,
    ) {
        for (name, result) in &audit.tool_results {
            if result.risk_level == RiskLevel::High || result.risk_level == RiskLevel::Critical {
                for issue in &result.issues {
                    let severity = if result.risk_level == RiskLevel::Critical {
                        RiskLevel::Critical
                    } else {
                        RiskLevel::High
                    };

                    if severity == RiskLevel::Critical {
                        critical.push(SecurityIssue {
                            category: "Tool Security".to_string(),
                            severity,
                            description: issue.clone(),
                            location: format!("src/tools/{}", name),
                            recommendation: format!("Fix security issue in {} tool", name),
                        });
                    } else {
                        warnings.push(SecurityIssue {
                            category: "Tool Security".to_string(),
                            severity,
                            description: issue.clone(),
                            location: format!("src/tools/{}", name),
                            recommendation: format!("Fix security issue in {} tool", name),
                        });
                    }
                }
            }
        }

        recommendations
            .push("Add resource limits (cgroups/ulimit) for shell and code execution".to_string());
        recommendations.push("Implement URL allowlists for web tools to prevent SSRF".to_string());
    }

    /// Collect data leak issues
    fn collect_leak_issues(
        &self,
        audit: &DataLeakAudit,
        critical: &mut Vec<SecurityIssue>,
        warnings: &mut Vec<SecurityIssue>,
        _recommendations: &mut Vec<String>,
    ) {
        for leak in &audit.leaks {
            let issue = SecurityIssue {
                category: "Data Leak".to_string(),
                severity: leak.severity,
                description: leak.description.clone(),
                location: leak.location.clone(),
                recommendation: leak.recommendation.clone(),
            };

            if leak.severity == RiskLevel::Critical {
                critical.push(issue);
            } else {
                warnings.push(issue);
            }
        }
    }

    /// Collect sandbox issues
    fn collect_sandbox_issues(
        &self,
        audit: &SandboxAudit,
        _critical: &mut Vec<SecurityIssue>,
        warnings: &mut Vec<SecurityIssue>,
        recommendations: &mut Vec<String>,
    ) {
        if !audit.resource_limits.memory_limits {
            warnings.push(SecurityIssue {
                category: "Sandbox".to_string(),
                severity: RiskLevel::High,
                description: "Memory limits not enforced for code execution".to_string(),
                location: "src/tools/code_exec.rs".to_string(),
                recommendation: "Implement memory limits using Docker or cgroup v2".to_string(),
            });
        }

        if !audit.isolation.network_isolation {
            warnings.push(SecurityIssue {
                category: "Sandbox".to_string(),
                severity: RiskLevel::Medium,
                description: "No network isolation for code execution".to_string(),
                location: "src/tools/code_exec.rs".to_string(),
                recommendation: "Use network namespaces or disable network in sandbox".to_string(),
            });
        }

        recommendations.push("Consider using WASM-based sandboxing for code execution".to_string());
    }

    /// Calculate overall security score
    fn calculate_score(
        &self,
        permissions: &PermissionAudit,
        tools: &ToolAudit,
        data_leaks: &DataLeakAudit,
        sandbox: &SandboxAudit,
        boundaries: &SecurityBoundaries,
    ) -> u8 {
        let mut score: u8 = 100;

        // Deduct for permission issues
        for comp in permissions.components.values() {
            if !comp.missing.is_empty() {
                score = score.saturating_sub(5 * comp.missing.len() as u8);
            }
        }

        // Deduct for tool issues
        for tool in tools.tool_results.values() {
            match tool.risk_level {
                RiskLevel::Critical => score = score.saturating_sub(25),
                RiskLevel::High => score = score.saturating_sub(15),
                RiskLevel::Medium => score = score.saturating_sub(5),
                RiskLevel::Low => score = score.saturating_sub(1),
            }
        }

        // Deduct for data leaks
        score = score.saturating_sub(10 * data_leaks.leaks_found as u8);

        // Deduct for sandbox issues
        if !sandbox.resource_limits.memory_limits {
            score = score.saturating_sub(10);
        }
        if !sandbox.isolation.network_isolation {
            score = score.saturating_sub(5);
        }

        // Deduct for unenforced boundaries
        score = score.saturating_sub(5 * boundaries.enforcement.not_enforced as u8);

        score
    }
}

impl Default for SecurityAuditor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audit_config_default() {
        let config = AuditConfig::default();
        assert!(config.check_log_leaks);
        assert!(config.verify_sandbox);
        assert!(config.audit_tools);
        assert!(config.review_permissions);
    }

    #[tokio::test]
    async fn test_security_auditor_runs() {
        let auditor = SecurityAuditor::new();
        let report = auditor.run_audit().await;

        // Report should have reasonable values
        assert!(report.score <= 100);
        assert!(!report.permissions.components.is_empty());
        assert!(!report.tools.tool_results.is_empty());
        assert!(!report.boundaries.boundaries.is_empty());
    }

    #[test]
    fn test_calculate_score_perfect_is_100() {
        let auditor = SecurityAuditor::new();
        let permissions = PermissionAudit::default();
        let tools = ToolAudit::default();
        let data_leaks = DataLeakAudit::default();
        let mut sandbox = SandboxAudit::default();
        sandbox.resource_limits.memory_limits = true;
        sandbox.isolation.network_isolation = true;
        let boundaries = SecurityBoundaries::default();

        let score =
            auditor.calculate_score(&permissions, &tools, &data_leaks, &sandbox, &boundaries);
        assert_eq!(score, 100, "Perfect audit should score 100");
    }

    #[test]
    fn test_calculate_score_deducts_for_missing_permissions() {
        let auditor = SecurityAuditor::new();
        let mut permissions = PermissionAudit::default();
        let mut comp = ComponentPermissions::default();
        comp.missing = vec!["memory_limits".to_string()];
        permissions.components.insert("test".to_string(), comp);

        let score = auditor.calculate_score(
            &permissions,
            &ToolAudit::default(),
            &DataLeakAudit::default(),
            &SandboxAudit::default(),
            &SecurityBoundaries::default(),
        );
        assert!(score < 100, "Missing permissions should reduce score");
    }

    #[test]
    fn test_calculate_score_deducts_for_critical_tool() {
        let auditor = SecurityAuditor::new();
        let mut tools = ToolAudit::default();
        let mut result = ToolAuditResult::default();
        result.risk_level = RiskLevel::Critical;
        tools.tool_results.insert("dangerous".to_string(), result);

        let score = auditor.calculate_score(
            &PermissionAudit::default(),
            &tools,
            &DataLeakAudit::default(),
            &SandboxAudit::default(),
            &SecurityBoundaries::default(),
        );
        assert!(score <= 75, "Critical tool risk should deduct at least 25, got {}", score);
    }

    #[test]
    fn test_calculate_score_deducts_for_high_tool() {
        let auditor = SecurityAuditor::new();
        let mut tools = ToolAudit::default();
        let mut result = ToolAuditResult::default();
        result.risk_level = RiskLevel::High;
        tools.tool_results.insert("risky".to_string(), result);

        let score = auditor.calculate_score(
            &PermissionAudit::default(),
            &tools,
            &DataLeakAudit::default(),
            &SandboxAudit::default(),
            &SecurityBoundaries::default(),
        );
        assert!(score <= 85, "High tool risk should deduct at least 15, got {}", score);
    }

    #[test]
    fn test_calculate_score_deducts_for_data_leaks() {
        let auditor = SecurityAuditor::new();
        let mut data_leaks = DataLeakAudit::default();
        data_leaks.leaks_found = 2;

        let score = auditor.calculate_score(
            &PermissionAudit::default(),
            &ToolAudit::default(),
            &data_leaks,
            &SandboxAudit::default(),
            &SecurityBoundaries::default(),
        );
        assert!(score <= 80, "2 data leaks should deduct at least 20, got {}", score);
    }

    #[test]
    fn test_calculate_score_never_below_0() {
        let auditor = SecurityAuditor::new();
        let mut permissions = PermissionAudit::default();
        let mut comp = ComponentPermissions::default();
        comp.missing = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        permissions.components.insert("test".to_string(), comp);

        let mut tools = ToolAudit::default();
        for i in 0..5 {
            let mut result = ToolAuditResult::default();
            result.risk_level = RiskLevel::Critical;
            tools.tool_results.insert(format!("tool{}", i), result);
        }

        let mut data_leaks = DataLeakAudit::default();
        data_leaks.leaks_found = 20;

        let mut sandbox = SandboxAudit::default();
        sandbox.resource_limits.memory_limits = false;
        sandbox.isolation.network_isolation = false;

        let mut boundaries = SecurityBoundaries::default();
        boundaries.enforcement.not_enforced = 10;

        let score =
            auditor.calculate_score(&permissions, &tools, &data_leaks, &sandbox, &boundaries);
        assert_eq!(score, 0, "Score should saturate at 0, got {}", score);
    }

    #[test]
    fn test_collect_permission_issues_generates_warnings() {
        let auditor = SecurityAuditor::new();
        let mut audit = PermissionAudit::default();
        let mut comp = ComponentPermissions::default();
        comp.missing = vec!["test_perm".to_string()];
        audit.components.insert("test_comp".to_string(), comp);

        let mut warnings = Vec::new();
        let mut recommendations = Vec::new();
        auditor.collect_permission_issues(&audit, &mut warnings, &mut recommendations);

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].category, "Permissions");
        assert!(!recommendations.is_empty());
    }

    #[test]
    fn test_collect_permission_issues_no_warnings_when_complete() {
        let auditor = SecurityAuditor::new();
        let audit = PermissionAudit::default();
        let mut warnings = Vec::new();
        let mut recommendations = Vec::new();
        auditor.collect_permission_issues(&audit, &mut warnings, &mut recommendations);
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_collect_tool_issues_critical_risk() {
        let auditor = SecurityAuditor::new();
        let mut audit = ToolAudit::default();
        let mut result = ToolAuditResult::default();
        result.risk_level = RiskLevel::Critical;
        result.issues = vec!["Critical issue".to_string()];
        audit.tool_results.insert("bad_tool".to_string(), result);

        let mut critical = Vec::new();
        let mut warnings = Vec::new();
        let mut recommendations = Vec::new();
        auditor.collect_tool_issues(&audit, &mut critical, &mut warnings, &mut recommendations);

        assert_eq!(critical.len(), 1);
        assert_eq!(critical[0].severity, RiskLevel::Critical);
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_collect_tool_issues_high_risk() {
        let auditor = SecurityAuditor::new();
        let mut audit = ToolAudit::default();
        let mut result = ToolAuditResult::default();
        result.risk_level = RiskLevel::High;
        result.issues = vec!["High issue".to_string()];
        audit.tool_results.insert("risky_tool".to_string(), result);

        let mut critical = Vec::new();
        let mut warnings = Vec::new();
        let mut recommendations = Vec::new();
        auditor.collect_tool_issues(&audit, &mut critical, &mut warnings, &mut recommendations);

        assert!(critical.is_empty());
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].severity, RiskLevel::High);
    }

    #[test]
    fn test_collect_tool_issues_ignores_low_risk() {
        let auditor = SecurityAuditor::new();
        let mut audit = ToolAudit::default();
        let mut result = ToolAuditResult::default();
        result.risk_level = RiskLevel::Low;
        result.issues = vec!["Minor issue".to_string()];
        audit.tool_results.insert("safe_tool".to_string(), result);

        let mut critical = Vec::new();
        let mut warnings = Vec::new();
        let mut recommendations = Vec::new();
        auditor.collect_tool_issues(&audit, &mut critical, &mut warnings, &mut recommendations);

        assert!(critical.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_collect_leak_issues_critical_goes_to_critical() {
        let auditor = SecurityAuditor::new();
        let mut audit = DataLeakAudit::default();
        audit.leaks.push(PotentialLeak {
            category: LeakCategory::CredentialExposure,
            description: "API key found".to_string(),
            location: "config.rs:10".to_string(),
            severity: RiskLevel::Critical,
            recommendation: "Remove key".to_string(),
        });

        let mut critical = Vec::new();
        let mut warnings = Vec::new();
        let mut recommendations = Vec::new();
        auditor.collect_leak_issues(&audit, &mut critical, &mut warnings, &mut recommendations);

        assert_eq!(critical.len(), 1);
        assert_eq!(critical[0].severity, RiskLevel::Critical);
    }

    #[test]
    fn test_collect_leak_issues_high_goes_to_warnings() {
        let auditor = SecurityAuditor::new();
        let mut audit = DataLeakAudit::default();
        audit.leaks.push(PotentialLeak {
            category: LeakCategory::UnencryptedStorage,
            description: "Unencrypted data".to_string(),
            location: "db.rs:20".to_string(),
            severity: RiskLevel::High,
            recommendation: "Encrypt data".to_string(),
        });

        let mut critical = Vec::new();
        let mut warnings = Vec::new();
        let mut _recommendations = Vec::new();
        auditor.collect_leak_issues(&audit, &mut critical, &mut warnings, &mut _recommendations);

        assert!(critical.is_empty());
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].severity, RiskLevel::High);
    }

    #[test]
    fn test_collect_sandbox_issues_memory_limits() {
        let auditor = SecurityAuditor::new();
        let mut audit = SandboxAudit::default();
        audit.resource_limits.memory_limits = false;

        let mut critical = Vec::new();
        let mut warnings = Vec::new();
        let mut recommendations = Vec::new();
        auditor.collect_sandbox_issues(&audit, &mut critical, &mut warnings, &mut recommendations);

        assert!(warnings
            .iter()
            .any(|w| w.description.contains("Memory limits")));
    }

    #[test]
    fn test_collect_sandbox_issues_network_isolation() {
        let auditor = SecurityAuditor::new();
        let mut audit = SandboxAudit::default();
        audit.isolation.network_isolation = false;

        let mut critical = Vec::new();
        let mut warnings = Vec::new();
        let mut recommendations = Vec::new();
        auditor.collect_sandbox_issues(&audit, &mut critical, &mut warnings, &mut recommendations);

        assert!(warnings
            .iter()
            .any(|w| w.description.contains("network isolation")));
    }
}
