//! Registry-level sandbox interceptor.
//!
//! Unlike [`SandboxedTool`] (per-tool wrapper), `SandboxInterceptor` is
//! designed to be registered as a **policy hook** on [`ToolRegistry`]
//! so that *all* tool calls are evaluated through a single, centrally
//! configurable rule set.
//!
//! # Usage
//!
//! ```rust
//! use syscity::tools::sandbox_interceptor::SandboxInterceptor;
//!
//! # async fn demo() {
//! let interceptor = SandboxInterceptor::default();
//! let hook = interceptor.as_policy_hook();
//! let decision = hook("shell", &serde_json::json!({"command": "echo hello"})).await;
//! assert!(decision.is_allow());
//! # }
//! ```

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use regex::Regex;
use serde_json::Value;

use super::hooks::ToolPolicyDecision;

#[allow(clippy::unwrap_used)]
static PATH_BLACKLIST: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        // SSH keys
        Regex::new(r"(?i)^.*/\.ssh(/|$)").unwrap(),
        // System credential stores
        Regex::new(r"(?i)^.*/\.gnupg(/|$)").unwrap(),
        Regex::new(r"(?i)^.*/\.aws(/|$)").unwrap(),
        Regex::new(r"(?i)^.*/\.docker(/|$)").unwrap(),
        Regex::new(r"(?i)^.*/\.kube(/|$)").unwrap(),
        Regex::new(r"(?i)^.*/\.config/gcloud(/|$)").unwrap(),
        Regex::new(r"(?i)^.*/\.config/azure(/|$)").unwrap(),
        Regex::new(r"(?i)^.*/\.netrc$").unwrap(),
        Regex::new(r"(?i)^.*/\.git-credentials$").unwrap(),
        // System directories (absolute paths only)
        Regex::new(r"^/etc(/|$)").unwrap(),
        Regex::new(r"^/sys(/|$)").unwrap(),
        Regex::new(r"^/proc(/|$)").unwrap(),
        Regex::new(r"^/dev(/|$)").unwrap(),
        Regex::new(r"^/boot(/|$)").unwrap(),
        Regex::new(r"^/var/log(/|$)").unwrap(),
        // Windows system directories
        Regex::new(r"(?i)^[A-Z]:\\Windows(/|\\|$)").unwrap(),
        Regex::new(r"(?i)^[A-Z]:\\Program Files(/|\\|$)").unwrap(),
        Regex::new(r"(?i)^[A-Z]:\\ProgramData(/|\\|$)").unwrap(),
    ]
});

/// Violation detected by the sandbox interceptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxError {
    /// A disallowed command was detected.
    CommandBlocked { command: String, reason: String },
    /// Access to a blocked path was attempted.
    PathBlocked { path: String, pattern: String },
    /// A network request to a non-allowlisted domain was attempted.
    NetworkBlocked { domain: String, reason: String },
    /// A network request to a blocked IP address or CIDR range was attempted.
    IpBlocked { ip: String, reason: String },
    /// Sensitive content was detected (does not block, flags for review).
    SensitiveDetected { kind: String, detail: String },
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::CommandBlocked { command, reason } => {
                write!(f, "Command '{}' blocked: {}", command, reason)
            }
            SandboxError::PathBlocked { path, pattern } => {
                write!(f, "Path '{}' blocked by pattern '{}'", path, pattern)
            }
            SandboxError::NetworkBlocked { domain, reason } => {
                write!(f, "Network domain '{}' blocked: {}", domain, reason)
            }
            SandboxError::IpBlocked { ip, reason } => {
                write!(f, "IP address '{}' blocked: {}", ip, reason)
            }
            SandboxError::SensitiveDetected { kind, detail } => {
                write!(f, "Sensitive content detected ({}): {}", kind, detail)
            }
        }
    }
}

impl std::error::Error for SandboxError {}

/// Centralised sandbox configuration.
#[derive(Debug, Clone)]
pub struct SandboxInterceptor {
    /// Commands that are unconditionally blocked.
    ///
    /// The check looks at the first whitespace-separated token of any
    /// `command` or `script` argument.
    command_blacklist: HashSet<String>,
    /// Regex patterns that block file-system access.
    path_blacklist: Vec<Regex>,
    /// If non-empty, only these domains may be accessed by network tools.
    domain_allowlist: Vec<String>,
    /// If non-empty, only IP addresses in these CIDR ranges may be accessed.
    /// Example: ["127.0.0.0/8", "10.0.0.0/8"]. When empty, no IP restriction.
    ip_allowlist: Vec<String>,
    /// IP addresses or CIDR ranges that are unconditionally blocked.
    /// Example: ["192.168.1.0/24", "10.0.0.5"].
    ip_blocklist: Vec<String>,
    /// If non-empty, only paths under these prefixes are permitted.
    /// When both `path_allowlist` and `path_blacklist` are configured,
    /// the allowlist is checked first (path must match at least one
    /// allowlist entry) and then the blacklist is applied.
    path_allowlist: Vec<PathBuf>,
    /// When `true`, the interceptor returns `NeedsApproval` instead of
    /// `Deny` for sensitive-content detections.
    flag_sensitive_for_approval: bool,
}

impl Default for SandboxInterceptor {
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl SandboxInterceptor {
    /// Create an interceptor with a sensible default rule set.
    pub fn with_defaults() -> Self {
        let mut command_blacklist = HashSet::new();
        // Dangerous system commands
        for cmd in [
            "rm",
            "dd",
            "fdisk",
            "mkfs",
            "format",
            "mkfs.ext4",
            "mkfs.xfs",
            "mkfs.ntfs",
            "parted",
            "gdisk",
            "sgdisk",
            "wipefs",
            "shred",
            "mkswap",
            "swapon",
            "swapoff",
            "sysctl",
            "modprobe",
            "rmmod",
            "insmod",
            "depmod",
            "lsmod",
            "kmod",
            "dmesg",
            "reboot",
            "shutdown",
            "poweroff",
            "halt",
            "init",
            "systemctl",
            "service",
            "chkconfig",
            "update-rc.d",
            "rc-update",
            "telinit",
            "runlevel",
            "killall5",
            "pkill",
            "skill",
            "snice",
            "chsh",
            "chfn",
            "vigr",
            "vipw",
            "usermod",
            "userdel",
            "groupmod",
            "groupdel",
            "gpasswd",
            "chpasswd",
            "newusers",
            "pwconv",
            "pwunconv",
            "grpconv",
            "grpunconv",
            "pwck",
            "grpck",
            "lastlog",
            "faillog",
        ] {
            command_blacklist.insert(cmd.to_string());
        }

        let path_blacklist = PATH_BLACKLIST.clone();

        Self {
            command_blacklist,
            path_blacklist,
            domain_allowlist: vec![],
            ip_allowlist: vec![],
            ip_blocklist: vec![],
            path_allowlist: vec![],
            flag_sensitive_for_approval: true,
        }
    }

    /// Add a command to the blacklist.
    pub fn block_command(mut self, command: impl Into<String>) -> Self {
        self.command_blacklist.insert(command.into());
        self
    }

    /// Add a regex pattern to the path blacklist.
    pub fn block_path_pattern(mut self, pattern: &str) -> crate::Result<Self> {
        let re = Regex::new(pattern)
            .map_err(|e| crate::error::SyscityError::Validation(format!("Invalid regex: {}", e)))?;
        self.path_blacklist.push(re);
        Ok(self)
    }

    /// Restrict network access to the given domains only.
    pub fn allow_domains(mut self, domains: Vec<String>) -> Self {
        self.domain_allowlist = domains;
        self
    }

    /// Restrict network access to the given IP ranges (CIDR notation) only.
    ///
    /// When non-empty, any IP address in tool arguments must fall within
    /// at least one of the listed CIDR ranges.
    pub fn allow_ip_ranges(mut self, ranges: Vec<String>) -> Self {
        self.ip_allowlist = ranges;
        self
    }

    /// Block specific IP addresses or CIDR ranges unconditionally.
    pub fn block_ip_ranges(mut self, ranges: Vec<String>) -> Self {
        self.ip_blocklist = ranges;
        self
    }

    /// Restrict file access to the given path prefixes only.
    ///
    /// When the allowlist is non-empty, any path argument must be a
    /// descendant of at least one allowed prefix.  The blacklist is
    /// still applied afterwards.
    pub fn allow_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.path_allowlist = paths;
        self
    }

    /// Evaluate a tool call against the sandbox rules.
    ///
    /// Returns `Ok(())` if the call passes all checks, or `Err(SandboxError)`
    /// if a rule is violated.
    pub fn check(&self, name: &str, args: &Value) -> Result<(), SandboxError> {
        // 1. Command blacklist
        if let Some(cmd) = extract_command(args) {
            let base = cmd.split_whitespace().next().unwrap_or("");
            if self.command_blacklist.contains(base) {
                return Err(SandboxError::CommandBlocked {
                    command: base.to_string(),
                    reason: format!("'{}' is on the command blacklist", base),
                });
            }
        }

        // 2. Path allowlist (checked first, if configured)
        let paths = extract_paths(args);
        if !self.path_allowlist.is_empty() {
            for path in &paths {
                let path_buf = PathBuf::from(path);
                let allowed = self.path_allowlist.iter().any(|a| path_buf.starts_with(a));
                if !allowed {
                    return Err(SandboxError::PathBlocked {
                        path: path.clone(),
                        pattern: format!("not in allowlist: {:?}", self.path_allowlist),
                    });
                }
            }
        }

        // 3. Path blacklist
        for path in paths {
            for pattern in &self.path_blacklist {
                if pattern.is_match(&path) {
                    return Err(SandboxError::PathBlocked {
                        path,
                        pattern: pattern.as_str().to_string(),
                    });
                }
            }
        }

        // 4. Network domain allowlist
        if !self.domain_allowlist.is_empty() {
            if let Some(domain) = extract_domain(args) {
                let allowed = self
                    .domain_allowlist
                    .iter()
                    .any(|d| domain.ends_with(d) || domain == *d);
                if !allowed {
                    return Err(SandboxError::NetworkBlocked {
                        domain,
                        reason: "Domain not in allowlist".to_string(),
                    });
                }
            }
        }

        // 5. IP range allowlist / blocklist
        if let Some(ip) = extract_ip(args) {
            // Blocklist checked first (deny always wins)
            for blocked in &self.ip_blocklist {
                if ip_in_range(&ip, blocked) {
                    return Err(SandboxError::IpBlocked {
                        ip: ip.clone(),
                        reason: format!("matches blocklist entry '{}'", blocked),
                    });
                }
            }
            // Allowlist checked second
            if !self.ip_allowlist.is_empty() {
                let allowed = self
                    .ip_allowlist
                    .iter()
                    .any(|range| ip_in_range(&ip, range));
                if !allowed {
                    return Err(SandboxError::IpBlocked {
                        ip: ip.clone(),
                        reason: format!("not in allowlist: {:?}", self.ip_allowlist),
                    });
                }
            }
        }

        // 6. Sensitive content detection (does not block, just flags)
        if let Some(detection) = detect_sensitive_content(name, args) {
            return Err(detection);
        }

        Ok(())
    }

    /// Convert this interceptor into a policy hook that can be registered
    /// with [`ToolHooks`].
    pub fn as_policy_hook(
        self,
    ) -> impl Fn(
        &str,
        &Value,
    )
        -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolPolicyDecision> + Send>> {
        let this = Arc::new(self);
        move |name: &str, args: &Value| {
            let this = Arc::clone(&this);
            let name = name.to_string();
            let args = args.clone();
            Box::pin(async move { this.evaluate(&name, &args) })
        }
    }

    /// Evaluate a tool call and return a [`ToolPolicyDecision`].
    fn evaluate(&self, name: &str, args: &Value) -> ToolPolicyDecision {
        match self.check(name, args) {
            Ok(()) => ToolPolicyDecision::Allow,
            Err(SandboxError::CommandBlocked { command, reason }) => ToolPolicyDecision::Deny {
                reason: format!("Sandbox: command '{}' is blocked — {}", command, reason),
            },
            Err(SandboxError::PathBlocked { path, pattern }) => ToolPolicyDecision::Deny {
                reason: format!("Sandbox: path '{}' matches blocked pattern '{}'", path, pattern),
            },
            Err(SandboxError::NetworkBlocked { domain, reason }) => ToolPolicyDecision::Deny {
                reason: format!("Sandbox: network domain '{}' is blocked — {}", domain, reason),
            },
            Err(SandboxError::IpBlocked { ip, reason }) => ToolPolicyDecision::Deny {
                reason: format!("Sandbox: IP address '{}' is blocked — {}", ip, reason),
            },
            Err(SandboxError::SensitiveDetected { kind, detail }) => {
                if self.flag_sensitive_for_approval {
                    ToolPolicyDecision::NeedsApproval {
                        approval_id: format!("sandbox-{}", uuid::Uuid::new_v4()),
                        tool_name: name.to_string(),
                        args: args.clone(),
                        risk_level: super::RiskLevel::High,
                        requested_by: "sandbox_interceptor".to_string(),
                        message: format!("Sensitive content detected ({}): {}", kind, detail),
                    }
                } else {
                    ToolPolicyDecision::Deny {
                        reason: format!(
                            "Sandbox: sensitive content detected ({}): {}",
                            kind, detail
                        ),
                    }
                }
            }
        }
    }
}

// ── Argument extractors ─────────────────────────────────────────────────────

/// Try to extract a shell command from tool arguments.
fn extract_command(args: &Value) -> Option<String> {
    // Common fields that hold commands or scripts
    for field in ["command", "script", "cmd", "shell", "bash"] {
        if let Some(s) = args.get(field).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

/// Extract all path-like strings from tool arguments.
fn extract_paths(args: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    const PATH_FIELDS: &[&str] = &[
        "path",
        "file",
        "directory",
        "dir",
        "source",
        "destination",
        "dst",
        "src",
        "from",
        "to",
        "output",
        "input",
        "out",
        "in",
    ];

    for field in PATH_FIELDS {
        if let Some(s) = args.get(field).and_then(|v| v.as_str()) {
            paths.push(s.to_string());
        }
    }

    // Also scan nested objects for path fields (e.g. in arrays or objects)
    if let Some(obj) = args.as_object() {
        for (_key, value) in obj {
            if let Some(s) = value.as_str() {
                if s.starts_with('/') || s.starts_with('~') || s.starts_with("./") {
                    paths.push(s.to_string());
                }
                // Windows paths
                if s.len() > 2 && s.as_bytes()[1] == b':' {
                    if let Some(c) = s.chars().next() {
                        if c.is_ascii_alphabetic() {
                            paths.push(s.to_string());
                        }
                    }
                }
            }
        }
    }

    paths
}

/// Try to extract a domain from URL-like arguments.
fn extract_domain(args: &Value) -> Option<String> {
    for field in ["url", "endpoint", "host", "domain", "base_url"] {
        if let Some(s) = args.get(field).and_then(|v| v.as_str()) {
            if let Some(domain) = parse_domain(s) {
                return Some(domain);
            }
        }
    }
    None
}

/// Parse a domain from a URL or plain domain string.
fn parse_domain(s: &str) -> Option<String> {
    // Handle URLs
    if let Some(rest) = s.strip_prefix("http://") {
        return Some(extract_host(rest));
    }
    if let Some(rest) = s.strip_prefix("https://") {
        return Some(extract_host(rest));
    }
    // Plain domain or IP
    if s.contains('.') || s.parse::<std::net::IpAddr>().is_ok() {
        return Some(extract_host(s));
    }
    None
}

/// Extract the host portion (before any path or port).
fn extract_host(s: &str) -> String {
    let without_path = s.split('/').next().unwrap_or(s);
    let host = without_path.split(':').next().unwrap_or(without_path);
    host.to_lowercase()
}

/// Try to extract an IP address from tool arguments.
fn extract_ip(args: &Value) -> Option<String> {
    for field in ["url", "endpoint", "host", "ip", "address", "target"] {
        if let Some(s) = args.get(field).and_then(|v| v.as_str()) {
            if let Some(ip) = parse_ip(s) {
                return Some(ip);
            }
        }
    }
    None
}

/// Parse an IP address from a URL or plain string.
fn parse_ip(s: &str) -> Option<String> {
    // Handle URLs with scheme
    let without_scheme = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
        .or_else(|| s.strip_prefix("ftp://"))
        .or_else(|| s.strip_prefix("ssh://"))
        .unwrap_or(s);
    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    let host = if host_port.starts_with('[') {
        // IPv6 bracket notation: [::1]:8080 or [::1]
        host_port
            .split(']')
            .next()
            .unwrap_or(host_port)
            .strip_prefix('[')
            .unwrap_or(host_port)
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    };

    // Try to parse as IP
    if host.parse::<IpAddr>().is_ok() {
        return Some(host.to_string());
    }
    None
}

/// Check if an IP address is within a CIDR range or matches exactly.
fn ip_in_range(ip: &str, range: &str) -> bool {
    let ip_addr = match ip.parse::<IpAddr>() {
        Ok(a) => a,
        Err(_) => return false,
    };

    // Exact match
    if ip == range {
        return true;
    }

    // CIDR notation
    let (network, prefix_len) = match range.split_once('/') {
        Some((net, prefix)) => {
            let len = match prefix.parse::<u8>() {
                Ok(n) => n,
                Err(_) => return false,
            };
            (net, len)
        }
        None => return false,
    };

    let network_addr = match network.parse::<IpAddr>() {
        Ok(a) => a,
        Err(_) => return false,
    };

    match (ip_addr, network_addr) {
        (IpAddr::V4(ip), IpAddr::V4(network)) => {
            let ip_u32 = u32::from(ip);
            let net_u32 = u32::from(network);
            let mask = if prefix_len == 0 {
                0u32
            } else {
                (!0u32) << (32 - prefix_len)
            };
            (ip_u32 & mask) == (net_u32 & mask)
        }
        (IpAddr::V6(ip), IpAddr::V6(network)) => {
            let ip_u128 = u128::from(ip);
            let net_u128 = u128::from(network);
            let mask = if prefix_len == 0 {
                0u128
            } else {
                (!0u128) << (128 - prefix_len)
            };
            (ip_u128 & mask) == (net_u128 & mask)
        }
        _ => false,
    }
}

/// Detect sensitive content in tool arguments.
fn detect_sensitive_content(_name: &str, args: &Value) -> Option<SandboxError> {
    let json_str = args.to_string();

    // API key patterns
    if json_str.contains("api_key") || json_str.contains("apikey") {
        return Some(SandboxError::SensitiveDetected {
            kind: "api_key".to_string(),
            detail: "Argument contains 'api_key' or 'apikey'".to_string(),
        });
    }

    // Password patterns
    if json_str.contains("password") || json_str.contains("passwd") {
        return Some(SandboxError::SensitiveDetected {
            kind: "password".to_string(),
            detail: "Argument contains 'password' or 'passwd'".to_string(),
        });
    }

    // Secret / token patterns
    if json_str.contains("secret") || json_str.contains("token") {
        return Some(SandboxError::SensitiveDetected {
            kind: "secret".to_string(),
            detail: "Argument contains 'secret' or 'token'".to_string(),
        });
    }

    // Private key
    if json_str.contains("private_key") || json_str.contains("privkey") {
        return Some(SandboxError::SensitiveDetected {
            kind: "private_key".to_string(),
            detail: "Argument contains 'private_key' or 'privkey'".to_string(),
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn test_command_blacklist_blocks_rm() {
        let interceptor = SandboxInterceptor::with_defaults();
        let args = json!({"command": "rm -rf /"});
        let err = interceptor.check("shell", &args).unwrap_err();
        assert!(matches!(err, SandboxError::CommandBlocked { command, .. } if command == "rm"));
    }

    #[test]
    fn test_command_blacklist_allows_echo() {
        let interceptor = SandboxInterceptor::with_defaults();
        let args = json!({"command": "echo hello"});
        assert!(interceptor.check("shell", &args).is_ok());
    }

    #[test]
    fn test_path_blacklist_blocks_ssh() {
        let interceptor = SandboxInterceptor::with_defaults();
        let args = json!({"path": "/home/user/.ssh/id_rsa"});
        let err = interceptor.check("file_read", &args).unwrap_err();
        assert!(matches!(err, SandboxError::PathBlocked { .. }));
    }

    #[test]
    fn test_path_blacklist_blocks_etc() {
        let interceptor = SandboxInterceptor::with_defaults();
        let args = json!({"path": "/etc/passwd"});
        let err = interceptor.check("file_read", &args).unwrap_err();
        assert!(matches!(err, SandboxError::PathBlocked { .. }));
    }

    #[test]
    fn test_path_blacklist_allows_tmp() {
        let interceptor = SandboxInterceptor::with_defaults();
        let args = json!({"path": "/tmp/test.txt"});
        assert!(interceptor.check("file_read", &args).is_ok());
    }

    #[test]
    fn test_domain_allowlist_blocks_unknown() {
        let interceptor =
            SandboxInterceptor::with_defaults().allow_domains(vec!["example.com".to_string()]);
        let args = json!({"url": "https://evil.com/api"});
        let err = interceptor.check("web_fetch", &args).unwrap_err();
        assert!(matches!(err, SandboxError::NetworkBlocked { domain, .. } if domain == "evil.com"));
    }

    #[test]
    fn test_domain_allowlist_allows_known() {
        let interceptor =
            SandboxInterceptor::with_defaults().allow_domains(vec!["example.com".to_string()]);
        let args = json!({"url": "https://example.com/api"});
        assert!(interceptor.check("web_fetch", &args).is_ok());
    }

    #[test]
    fn test_path_allowlist_blocks_outside() {
        let interceptor = SandboxInterceptor::with_defaults()
            .allow_paths(vec![PathBuf::from("/home/user/projects"), PathBuf::from("/tmp")]);
        let args = json!({"path": "/etc/passwd"});
        let err = interceptor.check("file_read", &args).unwrap_err();
        assert!(matches!(err, SandboxError::PathBlocked { .. }));
    }

    #[test]
    fn test_path_allowlist_allows_inside() {
        let interceptor = SandboxInterceptor::with_defaults()
            .allow_paths(vec![PathBuf::from("/home/user/projects"), PathBuf::from("/tmp")]);
        let args = json!({"path": "/tmp/test.txt"});
        assert!(interceptor.check("file_read", &args).is_ok());
    }

    #[test]
    fn test_path_allowlist_blocks_then_blacklist() {
        // Allowlist permits /home/user/projects, but blacklist blocks .ssh
        let interceptor =
            SandboxInterceptor::with_defaults().allow_paths(vec![PathBuf::from("/home/user")]);
        let args = json!({"path": "/home/user/.ssh/id_rsa"});
        let err = interceptor.check("file_read", &args).unwrap_err();
        assert!(matches!(err, SandboxError::PathBlocked { .. }));
    }

    #[test]
    fn test_sensitive_detects_api_key() {
        let interceptor = SandboxInterceptor::with_defaults();
        let args = json!({"text": "my api_key is secret123"});
        let err = interceptor.check("type", &args).unwrap_err();
        assert!(matches!(err, SandboxError::SensitiveDetected { kind, .. } if kind == "api_key"));
    }

    #[test]
    fn test_sensitive_allows_safe_text() {
        let interceptor = SandboxInterceptor::with_defaults();
        let args = json!({"text": "hello world"});
        assert!(interceptor.check("type", &args).is_ok());
    }

    #[test]
    fn test_extract_paths_from_nested() {
        let args = json!({"input": "/tmp/in.txt", "output": "/tmp/out.txt", "count": 5});
        let paths = extract_paths(&args);
        assert!(paths.contains(&"/tmp/in.txt".to_string()));
        assert!(paths.contains(&"/tmp/out.txt".to_string()));
    }

    #[test]
    fn test_parse_domain_url() {
        assert_eq!(parse_domain("https://example.com/path"), Some("example.com".to_string()));
        assert_eq!(
            parse_domain("http://api.example.com:8080/v1"),
            Some("api.example.com".to_string())
        );
        assert_eq!(parse_domain("example.com"), Some("example.com".to_string()));
    }

    #[tokio::test]
    async fn test_policy_hook_blocks_rm() {
        let interceptor = SandboxInterceptor::with_defaults();
        let hook = interceptor.as_policy_hook();
        let args = json!({"command": "rm -rf /"});
        let decision = hook("shell", &args).await;
        assert!(decision.is_deny());
    }

    #[tokio::test]
    async fn test_policy_hook_allows_safe() {
        let interceptor = SandboxInterceptor::with_defaults();
        let hook = interceptor.as_policy_hook();
        let args = json!({"command": "echo hello"});
        let decision = hook("shell", &args).await;
        assert!(decision.is_allow());
    }

    #[test]
    fn test_ip_blocklist_blocks_exact() {
        let interceptor =
            SandboxInterceptor::with_defaults().block_ip_ranges(vec!["192.168.1.5".to_string()]);
        let args = json!({"url": "http://192.168.1.5/api"});
        let err = interceptor.check("web_fetch", &args).unwrap_err();
        assert!(matches!(err, SandboxError::IpBlocked { ip, .. } if ip == "192.168.1.5"));
    }

    #[test]
    fn test_ip_blocklist_blocks_cidr() {
        let interceptor =
            SandboxInterceptor::with_defaults().block_ip_ranges(vec!["10.0.0.0/8".to_string()]);
        let args = json!({"host": "10.50.100.1"});
        let err = interceptor.check("shell", &args).unwrap_err();
        assert!(matches!(err, SandboxError::IpBlocked { ip, .. } if ip == "10.50.100.1"));
    }

    #[test]
    fn test_ip_allowlist_blocks_unknown() {
        let interceptor =
            SandboxInterceptor::with_defaults().allow_ip_ranges(vec!["127.0.0.0/8".to_string()]);
        let args = json!({"endpoint": "192.168.1.1"});
        let err = interceptor.check("web_fetch", &args).unwrap_err();
        assert!(matches!(err, SandboxError::IpBlocked { ip, .. } if ip == "192.168.1.1"));
    }

    #[test]
    fn test_ip_allowlist_allows_in_range() {
        let interceptor = SandboxInterceptor::with_defaults()
            .allow_ip_ranges(vec!["127.0.0.0/8".to_string(), "10.0.0.0/8".to_string()]);
        let args = json!({"url": "http://10.0.0.1:8080/path"});
        assert!(interceptor.check("web_fetch", &args).is_ok());
    }

    #[test]
    fn test_ip_blocklist_wins_over_allowlist() {
        // Blocklist is checked first, so a blocked IP in the allowlist range is still
        // denied
        let interceptor = SandboxInterceptor::with_defaults()
            .allow_ip_ranges(vec!["10.0.0.0/8".to_string()])
            .block_ip_ranges(vec!["10.1.0.0/16".to_string()]);
        let args = json!({"host": "10.1.50.1"});
        let err = interceptor.check("web_fetch", &args).unwrap_err();
        assert!(matches!(err, SandboxError::IpBlocked { ip, .. } if ip == "10.1.50.1"));
    }

    #[test]
    fn test_ip_check_allows_when_no_ip_restrictions() {
        let interceptor = SandboxInterceptor::with_defaults();
        let args = json!({"url": "http://192.168.1.1"});
        assert!(interceptor.check("web_fetch", &args).is_ok());
    }

    #[test]
    fn test_parse_ip_from_url() {
        assert_eq!(parse_ip("https://1.2.3.4/path"), Some("1.2.3.4".to_string()));
        assert_eq!(parse_ip("http://[::1]:8080"), Some("::1".to_string()));
        assert_eq!(parse_ip("example.com"), None);
    }

    #[test]
    fn test_ip_in_range_ipv4_cidr() {
        assert!(ip_in_range("192.168.1.5", "192.168.1.0/24"));
        assert!(!ip_in_range("192.168.2.5", "192.168.1.0/24"));
        assert!(ip_in_range("10.0.0.1", "10.0.0.0/8"));
        assert!(ip_in_range("10.255.255.255", "10.0.0.0/8"));
        assert!(!ip_in_range("11.0.0.1", "10.0.0.0/8"));
    }

    #[test]
    fn test_ip_in_range_exact_match() {
        assert!(ip_in_range("192.168.1.1", "192.168.1.1"));
        assert!(!ip_in_range("192.168.1.2", "192.168.1.1"));
    }

    #[test]
    fn test_ip_in_range_ipv6_cidr() {
        assert!(ip_in_range("2001:db8::1", "2001:db8::/32"));
        assert!(!ip_in_range("2001:db9::1", "2001:db8::/32"));
    }
}
