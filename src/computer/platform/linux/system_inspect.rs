//! System inspect tool — collect a structured system snapshot.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{debug, warn};

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Sections that can be inspected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectSection {
    Overview,
    Processes,
    Services,
    Network,
    Storage,
    Logs,
    Packages,
}

/// System snapshot result.
#[derive(Debug, Clone, Serialize)]
pub struct SystemSnapshot {
    pub hostname: String,
    pub uptime: String,
    pub load_average: [f64; 3],
    pub memory: MemoryInfo,
    pub cpu_count: usize,
    pub disks: Vec<DiskInfo>,
    pub processes: Vec<ProcessInfo>,
    pub services: Vec<ServiceInfo>,
    pub listening_ports: Vec<PortInfo>,
    pub recent_logs: Vec<LogEntry>,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryInfo {
    pub total_mb: u64,
    pub used_mb: u64,
    pub free_mb: u64,
    pub available_mb: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskInfo {
    pub filesystem: String,
    pub size: String,
    pub used: String,
    pub available: String,
    pub use_percent: String,
    pub mount: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub user: String,
    pub cpu_percent: f64,
    pub mem_percent: f64,
    pub command: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceInfo {
    pub name: String,
    pub state: String,
    pub sub_state: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PortInfo {
    pub protocol: String,
    pub local_address: String,
    pub state: String,
    pub process: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub unit: String,
    pub message: String,
}

/// Tool that collects a structured system snapshot.
#[derive(Debug)]
pub struct SystemInspectTool;

impl Default for SystemInspectTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemInspectTool {
    pub fn new() -> Self {
        Self
    }

    /// Run a shell command and return stdout as String.
    async fn run_cmd(cmd_str: &str, timeout_secs: u64) -> Option<String> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let result = timeout(
            Duration::from_secs(timeout_secs),
            Command::new(&shell).arg("-c").arg(cmd_str).output(),
        )
        .await;

        match result {
            Ok(Ok(output)) if output.status.success() => {
                Some(String::from_utf8_lossy(&output.stdout).to_string())
            }
            Ok(Ok(output)) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!("Command failed: {} — stderr: {}", cmd_str, stderr);
                None
            }
            Ok(Err(e)) => {
                warn!("Failed to spawn command '{}': {}", cmd_str, e);
                None
            }
            Err(_) => {
                warn!("Command timed out: {}", cmd_str);
                None
            }
        }
    }

    pub async fn collect_overview() -> (String, String, [f64; 3], MemoryInfo, usize) {
        let hostname_fut = Self::run_cmd("hostname", 5);
        let uptime_fut = Self::run_cmd("uptime -p", 5);
        let load_fut = Self::run_cmd("cat /proc/loadavg", 5);
        let mem_fut = Self::run_cmd("free -m", 5);
        let cpu_fut = Self::run_cmd("nproc", 5);

        let (hostname, uptime, load, mem, cpu) =
            tokio::join!(hostname_fut, uptime_fut, load_fut, mem_fut, cpu_fut);

        let hostname = hostname
            .unwrap_or_else(|| "unknown".to_string())
            .trim()
            .to_string();
        let uptime = uptime
            .unwrap_or_else(|| "unknown".to_string())
            .trim()
            .to_string();

        let load_avg = load
            .as_deref()
            .and_then(|s| {
                let parts: Vec<&str> = s.split_whitespace().collect();
                if parts.len() >= 3 {
                    Some([
                        parts[0].parse().unwrap_or(0.0),
                        parts[1].parse().unwrap_or(0.0),
                        parts[2].parse().unwrap_or(0.0),
                    ])
                } else {
                    None
                }
            })
            .unwrap_or([0.0, 0.0, 0.0]);

        let memory = mem
            .as_deref()
            .and_then(parse_free_output)
            .unwrap_or(MemoryInfo {
                total_mb: 0,
                used_mb: 0,
                free_mb: 0,
                available_mb: 0,
            });

        let cpu_count = cpu
            .as_deref()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        (hostname, uptime, load_avg, memory, cpu_count)
    }

    pub async fn collect_storage() -> Vec<DiskInfo> {
        let output = Self::run_cmd("df -hP", 10).await;
        output.as_deref().map(parse_df_output).unwrap_or_default()
    }

    pub async fn collect_processes(limit: usize) -> Vec<ProcessInfo> {
        let cmd = format!("ps aux --sort=-%cpu | head -n {}", limit + 1);
        let output = Self::run_cmd(&cmd, 10).await;
        output
            .as_deref()
            .map(|s| parse_ps_output(s, limit))
            .unwrap_or_default()
    }

    pub async fn collect_services(limit: usize) -> Vec<ServiceInfo> {
        let cmd = format!(
            "systemctl list-units --type=service --state=running --no-pager --no-legend | head -n \
             {}",
            limit
        );
        let output = Self::run_cmd(&cmd, 10).await;
        output
            .as_deref()
            .map(parse_systemctl_output)
            .unwrap_or_default()
    }

    pub async fn collect_network() -> Vec<PortInfo> {
        let output =
            Self::run_cmd("ss -tulnp --no-header 2>/dev/null || netstat -tulnp 2>/dev/null", 10)
                .await;
        output.as_deref().map(parse_ss_output).unwrap_or_default()
    }

    pub async fn collect_logs(lines: usize, since: &str) -> Vec<LogEntry> {
        let cmd = format!(
            "journalctl --no-pager --since '{}' -n {} 2>/dev/null || echo ''",
            shell_escape(since),
            lines
        );
        let output = Self::run_cmd(&cmd, 10).await;
        output
            .as_deref()
            .map(parse_journalctl_output)
            .unwrap_or_default()
    }
}

#[async_trait]
impl Tool for SystemInspectTool {
    fn name(&self) -> &str {
        "system_inspect"
    }

    fn description(&self) -> &str {
        "Collect a structured snapshot of the system state. Returns JSON with hostname, uptime, \
         load, memory, CPU, disks, processes, services, network ports, and recent logs. Use when \
         the user asks about system status, performance, or to diagnose server issues."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::observe(&["linux", "admin", "inspect"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Collect system snapshot",
            serde_json::json!({
                "sections": {
                    "type": "array",
                    "description": "Sections to inspect (default: all)",
                    "items": {
                        "type": "string",
                        "enum": ["overview", "processes", "services", "network", "storage", "logs"]
                    }
                },
                "process_limit": {
                    "type": "integer",
                    "description": "Max number of processes to return",
                    "default": 20
                },
                "service_limit": {
                    "type": "integer",
                    "description": "Max number of services to return",
                    "default": 20
                },
                "log_lines": {
                    "type": "integer",
                    "description": "Number of recent log lines",
                    "default": 30
                },
                "since": {
                    "type": "string",
                    "description": "Time range for logs (e.g. '1 hour ago')",
                    "default": "1 hour ago"
                }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let sections: Vec<String> = args["sections"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_else(|| vec!["overview".to_string()]);

        let all = sections.is_empty() || sections.iter().any(|s| s == "all");
        let wants = |name: &str| all || sections.iter().any(|s| s == name);

        let process_limit = args["process_limit"].as_u64().unwrap_or(20) as usize;
        let service_limit = args["service_limit"].as_u64().unwrap_or(20) as usize;
        let log_lines = args["log_lines"].as_u64().unwrap_or(30) as usize;
        let since = args["since"].as_str().unwrap_or("1 hour ago").to_string();

        debug!("system_inspect: sections={:?}", sections);

        // Collect overview (always included as base info)
        let (hostname, uptime, load_avg, memory, cpu_count) = Self::collect_overview().await;

        // Collect optional sections concurrently.
        let storage_fut = if wants("storage") || all {
            Some(Self::collect_storage())
        } else {
            None
        };
        let processes_fut = if wants("processes") || all {
            Some(Self::collect_processes(process_limit))
        } else {
            None
        };
        let services_fut = if wants("services") || all {
            Some(Self::collect_services(service_limit))
        } else {
            None
        };
        let network_fut = if wants("network") || all {
            Some(Self::collect_network())
        } else {
            None
        };
        let logs_fut = if wants("logs") || all {
            Some(Self::collect_logs(log_lines, &since))
        } else {
            None
        };

        let (disks, processes, services, listening_ports, recent_logs) = tokio::join!(
            async {
                match storage_fut {
                    Some(f) => f.await,
                    None => Vec::new(),
                }
            },
            async {
                match processes_fut {
                    Some(f) => f.await,
                    None => Vec::new(),
                }
            },
            async {
                match services_fut {
                    Some(f) => f.await,
                    None => Vec::new(),
                }
            },
            async {
                match network_fut {
                    Some(f) => f.await,
                    None => Vec::new(),
                }
            },
            async {
                match logs_fut {
                    Some(f) => f.await,
                    None => Vec::new(),
                }
            },
        );

        let snapshot = SystemSnapshot {
            hostname,
            uptime,
            load_average: load_avg,
            memory,
            cpu_count,
            disks,
            processes,
            services,
            listening_ports,
            recent_logs,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let json = serde_json::to_string_pretty(&snapshot)
            .map_err(crate::error::SyscityError::Serialization)?;

        Ok(ToolExecutionResult::success(json).with_data(serde_json::to_value(snapshot)?))
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        cfg!(target_os = "linux")
    }
}

// ── Output parsers ───────────────────────────────────────────────────────

fn parse_free_output(output: &str) -> Option<MemoryInfo> {
    for line in output.lines() {
        if line.starts_with("Mem:") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 7 {
                return Some(MemoryInfo {
                    total_mb: parts[1].parse().ok()?,
                    used_mb: parts[2].parse().ok()?,
                    free_mb: parts[3].parse().ok()?,
                    available_mb: parts[6].parse().ok()?,
                });
            }
        }
    }
    None
}

fn parse_df_output(output: &str) -> Vec<DiskInfo> {
    let mut disks = Vec::new();
    for line in output.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 6 {
            disks.push(DiskInfo {
                filesystem: parts[0].to_string(),
                size: parts[1].to_string(),
                used: parts[2].to_string(),
                available: parts[3].to_string(),
                use_percent: parts[4].to_string(),
                mount: parts[5].to_string(),
            });
        }
    }
    disks
}

fn parse_ps_output(output: &str, limit: usize) -> Vec<ProcessInfo> {
    let mut procs = Vec::new();
    for line in output.lines().skip(1).take(limit) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 11 {
            let pid = parts[1].parse().unwrap_or(0);
            let cpu = parts[2].parse().unwrap_or(0.0);
            let mem = parts[3].parse().unwrap_or(0.0);
            let cmd = parts[10..].join(" ");
            procs.push(ProcessInfo {
                pid,
                user: parts[0].to_string(),
                cpu_percent: cpu,
                mem_percent: mem,
                command: cmd,
            });
        }
    }
    procs
}

fn parse_systemctl_output(output: &str) -> Vec<ServiceInfo> {
    let mut services = Vec::new();
    for line in output.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 4 {
            let name = parts[0].to_string();
            let state = parts[3].to_string();
            let description = if parts.len() > 4 {
                parts[4..].join(" ")
            } else {
                String::new()
            };
            services.push(ServiceInfo {
                name,
                state,
                sub_state: parts.get(2).unwrap_or(&"").to_string(),
                description,
            });
        }
    }
    services
}

fn parse_ss_output(output: &str) -> Vec<PortInfo> {
    let mut ports = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // ss -tulnp output: tcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:*
        // users:(("sshd",pid=123,fd=3))
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() >= 5 {
            let protocol = parts[0].to_string();
            let state = parts.get(1).unwrap_or(&"").to_string();
            let local = parts.get(4).unwrap_or(&"").to_string();
            let process = if let Some(proc_idx) = parts.iter().position(|p| p.starts_with("users:"))
            {
                parts[proc_idx..].join(" ")
            } else {
                String::new()
            };
            ports.push(PortInfo {
                protocol,
                local_address: local,
                state,
                process,
            });
        }
    }
    ports
}

fn parse_journalctl_output(output: &str) -> Vec<LogEntry> {
    let mut logs = Vec::new();
    for line in output.lines() {
        // journalctl default format: Mon Jan 15 10:23:00 UTC 2024 hostname unit[pid]:
        // message Simplified: try to find timestamp + unit + message
        if line.len() > 30 {
            let ts = &line[..24]; // Rough timestamp extraction
            let rest = &line[24..];
            let unit_msg: Vec<&str> = rest.splitn(2, ':').collect();
            let (unit, msg) = if unit_msg.len() == 2 {
                (unit_msg[0].trim().to_string(), unit_msg[1].trim().to_string())
            } else {
                (String::new(), rest.trim().to_string())
            };
            logs.push(LogEntry {
                timestamp: ts.to_string(),
                unit,
                message: msg,
            });
        } else if !line.trim().is_empty() {
            logs.push(LogEntry {
                timestamp: String::new(),
                unit: String::new(),
                message: line.to_string(),
            });
        }
    }

    logs
}

fn shell_escape(input: &str) -> String {
    input.replace('"', "\\\"").replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_free_output() {
        let output = "              total        used        free      shared  buff/cache   \
                      available\nMem:          15984        8234        1234         456        \
                      6516        6789\nSwap:          2048           0        2048";
        let mem = parse_free_output(output).unwrap();
        assert_eq!(mem.total_mb, 15984);
        assert_eq!(mem.used_mb, 8234);
        assert_eq!(mem.available_mb, 6789);
    }

    #[test]
    fn test_parse_df_output() {
        let output = "Filesystem      Size  Used Avail Use% Mounted on\n/dev/sda1        98G   \
                      45G   48G  49% /\ntmpfs           7.9G  1.2M  7.9G   1% /run";
        let disks = parse_df_output(output);
        assert_eq!(disks.len(), 2);
        assert_eq!(disks[0].mount, "/");
    }
}
