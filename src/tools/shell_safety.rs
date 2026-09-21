//! Shell command safety analysis
//!
//! Provides a [`SafeBinList`] for pre-approved binary paths and a
//! [`shell_safety_policy`] policy hook that can auto-deny dangerous commands
//! and flag high-risk ones for human approval.
//!
//! ## Policy behaviour
//!
//! | Classification | Decision |
//! |---|---|
//! | Binary is in `safe_bins` | `Allow` |
//! | Auto-deny pattern matched (e.g. `rm -rf /`, `dd`, fork bomb) | `Deny` |
//! | High-risk pattern matched (e.g. `sudo`, `curl \| bash`) | `NeedsApproval` |
//! | Everything else | `Allow` |

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use crate::tools::approval::RiskLevel;
use crate::tools::hooks::{PolicyHookFn, ToolPolicyDecision};
use crate::tools::ToolContext;

/// A list of pre-approved binary names / paths.
///
/// Commands whose first token matches an entry in this list are allowed
/// without further scrutiny (unless they match an auto-deny pattern).
#[derive(Debug, Clone)]
pub struct SafeBinList {
    bins: HashSet<String>,
}

impl SafeBinList {
    /// Create a new `SafeBinList` with a default set of safe binaries.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an empty `SafeBinList`.
    pub fn empty() -> Self {
        Self { bins: HashSet::new() }
    }

    /// Add a binary name or path to the allowlist.
    pub fn allow(mut self, bin: impl Into<String>) -> Self {
        self.bins.insert(bin.into());
        self
    }

    /// Remove a binary from the allowlist.
    pub fn disallow(mut self, bin: &str) -> Self {
        self.bins.remove(bin);
        self
    }

    /// Check if a binary name is in the allowlist.
    pub fn is_allowed(&self, bin: &str) -> bool {
        self.bins.contains(bin)
    }
}

impl Default for SafeBinList {
    /// Default set of safe binaries considered low-risk for routine use.
    fn default() -> Self {
        Self {
            bins: HashSet::from([
                // File operations
                "ls".to_string(),
                "cat".to_string(),
                "head".to_string(),
                "tail".to_string(),
                "less".to_string(),
                "more".to_string(),
                "wc".to_string(),
                "sort".to_string(),
                "uniq".to_string(),
                "cut".to_string(),
                "grep".to_string(),
                "rg".to_string(),
                "awk".to_string(),
                "sed".to_string(),
                "find".to_string(),
                "diff".to_string(),
                "cmp".to_string(),
                // Version control
                "git".to_string(),
                // Network
                "curl".to_string(),
                "wget".to_string(),
                "ping".to_string(),
                "dig".to_string(),
                "nslookup".to_string(),
                "ssh".to_string(),
                // Python / scripting
                "python".to_string(),
                "python3".to_string(),
                "node".to_string(),
                "deno".to_string(),
                "bun".to_string(),
                "ruby".to_string(),
                "perl".to_string(),
                "sh".to_string(),
                "bash".to_string(),
                "zsh".to_string(),
                "fish".to_string(),
                // Build tools
                "cargo".to_string(),
                "make".to_string(),
                "cmake".to_string(),
                "ninja".to_string(),
                "npm".to_string(),
                "yarn".to_string(),
                "pnpm".to_string(),
                "pip".to_string(),
                "pip3".to_string(),
                // System info
                "ps".to_string(),
                "top".to_string(),
                "htop".to_string(),
                "df".to_string(),
                "du".to_string(),
                "uname".to_string(),
                "whoami".to_string(),
                "id".to_string(),
                "date".to_string(),
                "cal".to_string(),
                "which".to_string(),
                "env".to_string(),
                "printenv".to_string(),
                "echo".to_string(),
                "printf".to_string(),
                // File manipulation
                "cp".to_string(),
                "mv".to_string(),
                "mkdir".to_string(),
                "touch".to_string(),
                "chmod".to_string(),
                "chown".to_string(),
                "ln".to_string(),
                "tar".to_string(),
                "gzip".to_string(),
                "gunzip".to_string(),
                "xz".to_string(),
                "zip".to_string(),
                "unzip".to_string(),
                // Reading / viewing
                "file".to_string(),
                "stat".to_string(),
                "realpath".to_string(),
                "readlink".to_string(),
                "basename".to_string(),
                "dirname".to_string(),
                "strings".to_string(),
                "xxd".to_string(),
                "hexdump".to_string(),
                "od".to_string(),
                // Process management
                "kill".to_string(),
                "pkill".to_string(),
                "pgrep".to_string(),
                "nohup".to_string(),
                "timeout".to_string(),
            ]),
        }
    }
}

/// Classify a shell command into a safety tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellSafetyTier {
    /// Safe — no restrictions.
    Safe,
    /// High-risk — should require human approval.
    HighRisk,
    /// Critical — should be denied outright.
    Dangerous,
}

/// Patterns that are always dangerous.
const DANGEROUS_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -rf /*",
    "rm -fr /",
    "rm -fr /*",
    "rm -rf --no-preserve-root",
    "dd if=",
    "mkfs",
    "mkfs.ext",
    "mkfs.ext4",
    "mkfs.btrfs",
    "mkfs.xfs",
    "fdisk",
    "parted",
    "mkswap",
    "format",
    "> /dev/sda",
    "> /dev/hda",
    ":(){ :|:& };:",
    "forkbomb",
    "chmod -R 000 /",
    "chown -R root /",
    ":wq!",
    "wget -O - |",
    "curl -s | bash",
    "sh -c \"$(curl -fsSL",
    "bash <(curl",
    "sudo rm -rf",
];

/// Patterns that are high-risk and require approval.
const HIGH_RISK_PATTERNS: &[&str] = &[
    "sudo ",
    "su ",
    "iptables",
    "ufw",
    "systemctl",
    "journalctl",
    "docker",
    "kubectl",
    "passwd",
    "useradd",
    "userdel",
    "groupadd",
    "groupdel",
    "usermod",
    "visudo",
    "/etc/shadow",
    "/etc/sudoers",
    "/etc/passwd",
    "/etc/ssl",
    "gpg",
    "openssl",
    "crontab",
    "at ",
    "batch",
    "poweroff",
    "shutdown",
    "reboot",
    "halt",
    "init 0",
    "init 6",
    "telinit",
    "modprobe",
    "insmod",
    "rmmod",
    "sysctl -w",
    "echo > /proc",
    "write /proc",
    "chattr",
    "lsattr",
    "setfacl",
    "getfacl",
    "swapon",
    "swapoff",
    "mount",
    "umount",
];

/// Analyse a shell command and return its safety tier.
///
/// `command` is the raw shell command string (e.g. `"ls -la"`).
/// `safe_bins` is the list of pre-approved binaries.
pub fn analyze_shell_command(command: &str, safe_bins: &SafeBinList) -> ShellSafetyTier {
    let trimmed = command.trim();

    // 1. Check for dangerous patterns
    for pattern in DANGEROUS_PATTERNS {
        if trimmed.contains(pattern) {
            return ShellSafetyTier::Dangerous;
        }
    }

    // 2. Extract the first token (binary name)
    let first_token = trimmed.split_whitespace().next().unwrap_or("");

    // 3. If binary is in the safe list, allow
    if safe_bins.is_allowed(first_token) {
        return ShellSafetyTier::Safe;
    }

    // 4. Check for high-risk patterns
    for pattern in HIGH_RISK_PATTERNS {
        if trimmed.contains(pattern) {
            return ShellSafetyTier::HighRisk;
        }
    }

    // 5. Unknown binary — treat as high-risk by default
    if !first_token.is_empty() {
        ShellSafetyTier::HighRisk
    } else {
        ShellSafetyTier::Safe
    }
}

/// Create a policy hook that enforces shell safety rules.
///
/// The returned [`PolicyHookFn`] applies to any tool named `"shell"` /
/// `"bash"` / `"sh"` / `"cmd"` and uses the provided `safe_bins` list.
///
/// # Example
///
/// ```rust,no_run
/// use syscity::tools::hooks::ToolHooks;
/// use syscity::tools::shell_safety::{shell_safety_policy, SafeBinList};
///
/// let safe_bins = SafeBinList::new().allow("docker");
/// let policy = shell_safety_policy(safe_bins);
/// let hooks = ToolHooks::new().policy(move |name, args, ctx| policy(name, args, ctx));
/// ```
pub fn shell_safety_policy(safe_bins: SafeBinList) -> PolicyHookFn {
    std::sync::Arc::new(move |name: &str, args: &serde_json::Value, _ctx: &ToolContext| {
        let safe_bins = safe_bins.clone();
        let name = name.to_string();
        let args = args.clone();
        Box::pin(async move {
            // Only apply to shell-like tools
            if !matches!(name.as_str(), "shell" | "bash" | "sh" | "cmd") {
                return ToolPolicyDecision::Allow;
            }

            // Extract the command string from args (cloned to release borrow before move)
            let command = args
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            match analyze_shell_command(&command, &safe_bins) {
                ShellSafetyTier::Safe => ToolPolicyDecision::Allow,
                ShellSafetyTier::Dangerous => ToolPolicyDecision::Deny {
                    reason: format!(
                        "Shell command '{}' matches a dangerous pattern and is denied",
                        command
                    ),
                },
                ShellSafetyTier::HighRisk => ToolPolicyDecision::NeedsApproval {
                    approval_id: format!("shell-{}", uuid::Uuid::new_v4()),
                    tool_name: name,
                    args,
                    risk_level: RiskLevel::High,
                    requested_by: "system".into(),
                    message: format!("Shell command requires host approval: {}", command),
                },
            }
        }) as Pin<Box<dyn Future<Output = ToolPolicyDecision> + Send>>
    }) as PolicyHookFn
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safe_bin_list_default_contains_common_tools() {
        let safe = SafeBinList::new();
        assert!(safe.is_allowed("ls"));
        assert!(safe.is_allowed("git"));
        assert!(safe.is_allowed("curl"));
        assert!(safe.is_allowed("python3"));
        assert!(safe.is_allowed("cargo"));
    }

    #[test]
    fn test_safe_bin_list_empty() {
        let safe = SafeBinList::empty();
        assert!(!safe.is_allowed("ls"));
        assert!(!safe.is_allowed("git"));
    }

    #[test]
    fn test_safe_bin_list_allow_disallow() {
        let safe = SafeBinList::new().allow("mybin").disallow("rm");
        assert!(safe.is_allowed("mybin"));
        assert!(!safe.is_allowed("rm"));
    }

    #[test]
    fn test_dangerous_rm_rf() {
        let safe = SafeBinList::new();
        assert_eq!(analyze_shell_command("rm -rf /", &safe), ShellSafetyTier::Dangerous,);
        assert_eq!(analyze_shell_command("rm -rf /*", &safe), ShellSafetyTier::Dangerous,);
    }

    #[test]
    fn test_dangerous_dd() {
        let safe = SafeBinList::new();
        assert_eq!(
            analyze_shell_command("dd if=/dev/zero of=/dev/sda bs=1M", &safe),
            ShellSafetyTier::Dangerous,
        );
    }

    #[test]
    fn test_dangerous_fork_bomb() {
        let safe = SafeBinList::new();
        assert_eq!(analyze_shell_command(":(){ :|:& };:", &safe), ShellSafetyTier::Dangerous,);
    }

    #[test]
    fn test_high_risk_sudo() {
        let safe = SafeBinList::new();
        assert_eq!(analyze_shell_command("sudo apt update", &safe), ShellSafetyTier::HighRisk,);
    }

    #[test]
    fn test_high_risk_iptables() {
        let safe = SafeBinList::new();
        assert_eq!(analyze_shell_command("iptables -L", &safe), ShellSafetyTier::HighRisk,);
    }

    #[test]
    fn test_safe_known_binary() {
        let safe = SafeBinList::new();
        assert_eq!(analyze_shell_command("ls -la", &safe), ShellSafetyTier::Safe,);
        assert_eq!(analyze_shell_command("git status", &safe), ShellSafetyTier::Safe,);
    }

    #[test]
    fn test_unknown_binary_is_high_risk() {
        let safe = SafeBinList::new();
        assert_eq!(
            analyze_shell_command("some_unknown_tool --flag", &safe),
            ShellSafetyTier::HighRisk,
        );
    }

    #[test]
    fn test_empty_command_is_safe() {
        let safe = SafeBinList::new();
        assert_eq!(analyze_shell_command("", &safe), ShellSafetyTier::Safe,);
    }

    #[test]
    fn test_safe_bin_default() {
        let safe: SafeBinList = Default::default();
        assert!(safe.is_allowed("cat"));
        assert!(safe.is_allowed("grep"));
    }

    #[test]
    fn test_shell_safety_policy_non_shell_tool() {
        let safe = SafeBinList::new();
        let policy = shell_safety_policy(safe);
        let result = tokio::runtime::Runtime::new().unwrap().block_on(async {
            policy("read", &serde_json::json!({}), &ToolContext::default()).await
        });
        assert_eq!(result, ToolPolicyDecision::Allow);
    }

    #[test]
    fn test_shell_safety_policy_allows_ls() {
        let safe = SafeBinList::new();
        let policy = shell_safety_policy(safe);
        let result = tokio::runtime::Runtime::new().unwrap().block_on(async {
            policy("shell", &serde_json::json!({"command": "ls -la"}), &ToolContext::default())
                .await
        });
        assert!(result.is_allow());
    }

    #[test]
    fn test_shell_safety_policy_denies_rm_rf() {
        let safe = SafeBinList::new();
        let policy = shell_safety_policy(safe);
        let result = tokio::runtime::Runtime::new().unwrap().block_on(async {
            policy("shell", &serde_json::json!({"command": "rm -rf /"}), &ToolContext::default())
                .await
        });
        assert!(result.is_deny());
    }

    #[test]
    fn test_shell_safety_policy_needs_approval_for_sudo() {
        let safe = SafeBinList::new();
        let policy = shell_safety_policy(safe);
        let result = tokio::runtime::Runtime::new().unwrap().block_on(async {
            policy(
                "bash",
                &serde_json::json!({"command": "sudo apt update"}),
                &ToolContext::default(),
            )
            .await
        });
        assert!(result.is_needs_approval());
    }
}
