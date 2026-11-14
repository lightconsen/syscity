//! Package manager tool — query, install, remove, and update packages.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::warn;

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Action types for package management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageAction {
    List,
    Search,
    Install,
    Remove,
    Update,
}

/// Detected package manager backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageBackend {
    Apt,
    Dnf,
    Pacman,
    Apk,
    Zypper,
    Unknown,
}

impl PackageBackend {
    fn detect() -> Self {
        if std::process::Command::new("apt")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Self::Apt;
        }
        if std::process::Command::new("dnf")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Self::Dnf;
        }
        if std::process::Command::new("pacman")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Self::Pacman;
        }
        if std::process::Command::new("apk")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Self::Apk;
        }
        if std::process::Command::new("zypper")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Self::Zypper;
        }
        Self::Unknown
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Apt => "apt",
            Self::Dnf => "dnf",
            Self::Pacman => "pacman",
            Self::Apk => "apk",
            Self::Zypper => "zypper",
            Self::Unknown => "unknown",
        }
    }
}

/// A package entry.
#[derive(Debug, Clone, Serialize)]
pub struct PackageEntry {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub installed: bool,
}

/// Tool for managing packages on Linux.
#[derive(Debug)]
pub struct PackageManagerTool;

impl Default for PackageManagerTool {
    fn default() -> Self {
        Self::new()
    }
}

impl PackageManagerTool {
    pub fn new() -> Self {
        Self
    }

    async fn run_pkg_cmd(cmd: &str, args: &[&str], timeout_secs: u64) -> Option<(bool, String)> {
        let result =
            timeout(Duration::from_secs(timeout_secs), Command::new(cmd).args(args).output()).await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let combined = if stderr.is_empty() {
                    stdout
                } else {
                    format!("{stdout}\n{stderr}")
                };
                Some((output.status.success(), combined))
            }
            Ok(Err(e)) => {
                warn!("Failed to run {}: {}", cmd, e);
                None
            }
            Err(_) => {
                warn!("{} timed out", cmd);
                None
            }
        }
    }

    async fn do_list(backend: PackageBackend) -> Vec<PackageEntry> {
        let entries = match backend {
            PackageBackend::Apt => {
                match Self::run_pkg_cmd("dpkg", &["-l"], 30).await {
                    Some((true, output)) => output
                        .lines()
                        .skip(5) // Skip header
                        .filter_map(|line| {
                            let parts: Vec<&str> = line.split_whitespace().collect();
                            if parts.len() >= 3 {
                                Some(PackageEntry {
                                    name: parts[1].to_string(),
                                    version: parts[2].to_string(),
                                    description: None,
                                    installed: parts[0].starts_with('i'),
                                })
                            } else {
                                None
                            }
                        })
                        .collect(),
                    _ => Vec::new(),
                }
            }
            PackageBackend::Dnf => {
                match Self::run_pkg_cmd(
                    "rpm",
                    &["-qa", "--queryformat", "%{NAME}\t%{VERSION}\n"],
                    30,
                )
                .await
                {
                    Some((true, output)) => output
                        .lines()
                        .filter_map(|line| {
                            let parts: Vec<&str> = line.split('\t').collect();
                            if parts.len() >= 2 {
                                Some(PackageEntry {
                                    name: parts[0].to_string(),
                                    version: parts[1].to_string(),
                                    description: None,
                                    installed: true,
                                })
                            } else {
                                None
                            }
                        })
                        .collect(),
                    _ => Vec::new(),
                }
            }
            PackageBackend::Pacman => match Self::run_pkg_cmd("pacman", &["-Q"], 30).await {
                Some((true, output)) => output
                    .lines()
                    .filter_map(|line| {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() >= 2 {
                            Some(PackageEntry {
                                name: parts[0].to_string(),
                                version: parts[1].to_string(),
                                description: None,
                                installed: true,
                            })
                        } else {
                            None
                        }
                    })
                    .collect(),
                _ => Vec::new(),
            },
            PackageBackend::Apk => {
                match Self::run_pkg_cmd("apk", &["list", "--installed"], 30).await {
                    Some((true, output)) => output
                        .lines()
                        .filter_map(|line| {
                            let name_ver = line.split_whitespace().next()?;
                            let parts: Vec<&str> = name_ver.split('-').collect();
                            if parts.len() >= 2 {
                                Some(PackageEntry {
                                    name: parts[..parts.len() - 1].join("-"),
                                    version: parts.last()?.to_string(),
                                    description: None,
                                    installed: true,
                                })
                            } else {
                                None
                            }
                        })
                        .collect(),
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        };
        entries.into_iter().take(200).collect()
    }

    async fn do_search(backend: PackageBackend, query: &str) -> Vec<PackageEntry> {
        match backend {
            PackageBackend::Apt => {
                match Self::run_pkg_cmd("apt-cache", &["search", query], 30).await {
                    Some((true, output)) => output
                        .lines()
                        .filter_map(|line| {
                            line.find(" - ").map(|pos| PackageEntry {
                                name: line[..pos].trim().to_string(),
                                version: String::new(),
                                description: Some(line[pos + 3..].to_string()),
                                installed: false,
                            })
                        })
                        .take(50)
                        .collect(),
                    _ => Vec::new(),
                }
            }
            PackageBackend::Dnf => match Self::run_pkg_cmd("dnf", &["search", query], 60).await {
                Some((true, output)) => output
                    .lines()
                    .filter(|l| l.contains(query))
                    .map(|line| PackageEntry {
                        name: line.split_whitespace().next().unwrap_or("").to_string(),
                        version: String::new(),
                        description: None,
                        installed: line.contains("@"),
                    })
                    .take(50)
                    .collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    async fn do_install(backend: PackageBackend, package: &str) -> (bool, String) {
        let (cmd, args) = match backend {
            PackageBackend::Apt => ("apt-get", vec!["install", "-y", package]),
            PackageBackend::Dnf => ("dnf", vec!["install", "-y", package]),
            PackageBackend::Pacman => ("pacman", vec!["-S", "--noconfirm", package]),
            PackageBackend::Apk => ("apk", vec!["add", package]),
            _ => return (false, "Unknown package manager".to_string()),
        };
        match Self::run_pkg_cmd(cmd, &args.to_vec(), 300).await {
            Some((success, output)) => (success, output),
            None => (false, "Failed to execute package manager".to_string()),
        }
    }

    async fn do_remove(backend: PackageBackend, package: &str) -> (bool, String) {
        let (cmd, args) = match backend {
            PackageBackend::Apt => ("apt-get", vec!["remove", "-y", package]),
            PackageBackend::Dnf => ("dnf", vec!["remove", "-y", package]),
            PackageBackend::Pacman => ("pacman", vec!["-R", "--noconfirm", package]),
            PackageBackend::Apk => ("apk", vec!["del", package]),
            _ => return (false, "Unknown package manager".to_string()),
        };
        match Self::run_pkg_cmd(cmd, &args.to_vec(), 300).await {
            Some((success, output)) => (success, output),
            None => (false, "Failed to execute package manager".to_string()),
        }
    }

    async fn do_update(backend: PackageBackend) -> (bool, String) {
        let (cmd, args) = match backend {
            PackageBackend::Apt => ("apt-get", vec!["update"]),
            PackageBackend::Dnf => ("dnf", vec!["check-update"]),
            PackageBackend::Pacman => ("pacman", vec!["-Sy"]),
            PackageBackend::Apk => ("apk", vec!["update"]),
            _ => return (false, "Unknown package manager".to_string()),
        };
        match Self::run_pkg_cmd(cmd, &args.to_vec(), 300).await {
            Some((success, output)) => (success, output),
            None => (false, "Failed to execute package manager".to_string()),
        }
    }
}

#[async_trait]
impl Tool for PackageManagerTool {
    fn name(&self) -> &str {
        "package_manager"
    }

    fn description(&self) -> &str {
        "Manage software packages on Linux. Auto-detects apt, dnf, pacman, apk, or zypper. \
         Supports listing installed packages, searching, installing, removing, and updating."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::system_change(&["linux", "admin", "packages"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Manage packages",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "Action: list | search | install | remove | update",
                    "enum": ["list", "search", "install", "remove", "update"]
                },
                "package": {
                    "type": "string",
                    "description": "Package name for install, remove, or search actions"
                }
            }),
            vec!["action"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let action_str = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("list");
        let action = match action_str {
            "search" => PackageAction::Search,
            "install" => PackageAction::Install,
            "remove" => PackageAction::Remove,
            "update" => PackageAction::Update,
            _ => PackageAction::List,
        };

        let backend = PackageBackend::detect();
        if matches!(backend, PackageBackend::Unknown) {
            return Ok(ToolExecutionResult::error(
                "No supported package manager found (tried apt, dnf, pacman, apk, zypper)"
                    .to_string(),
            ));
        }

        let data = match action {
            PackageAction::List => {
                let packages = Self::do_list(backend).await;
                serde_json::json!({
                    "action": "list",
                    "backend": backend.as_str(),
                    "count": packages.len(),
                    "packages": packages,
                })
            }
            PackageAction::Search => {
                let query = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
                if query.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "'package' is required for search action".to_string(),
                    ));
                }
                let packages = Self::do_search(backend, query).await;
                serde_json::json!({
                    "action": "search",
                    "backend": backend.as_str(),
                    "query": query,
                    "count": packages.len(),
                    "packages": packages,
                })
            }
            PackageAction::Install | PackageAction::Remove | PackageAction::Update => {
                let package = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
                if action != PackageAction::Update && package.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "'package' is required for install/remove actions".to_string(),
                    ));
                }
                let (success, output) = match action {
                    PackageAction::Install => Self::do_install(backend, package).await,
                    PackageAction::Remove => Self::do_remove(backend, package).await,
                    PackageAction::Update => Self::do_update(backend).await,
                    _ => unreachable!(),
                };
                serde_json::json!({
                    "action": action_str,
                    "backend": backend.as_str(),
                    "package": package,
                    "success": success,
                    "output": output,
                })
            }
        };

        let message = format!("Package '{}' completed (backend: {})", action_str, backend.as_str());
        Ok(ToolExecutionResult::success(message).with_data(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_package_manager_tool_name() {
        let tool = PackageManagerTool::new();
        assert_eq!(tool.name(), "package_manager");
    }

    #[test]
    fn test_package_manager_schema() {
        let tool = PackageManagerTool::new();
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
    }
}
