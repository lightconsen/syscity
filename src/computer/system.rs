//! Cross-platform system monitoring and process management.
//!
//! Uses the `sysinfo` crate to provide structured CPU, memory, disk,
//! network, and process information without shelling out to `ps`, `df`,
//! `free`, or `tasklist`.

use std::time::Duration;

use sysinfo::{
    Disks, Networks, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, Signal, System,
};

use crate::computer::types::{DiskStatus, NetworkStatus, ProcessEntry, SystemStatus};
use crate::computer::{ComputerError, Result};

/// Lightweight system monitor backed by `sysinfo::System`.
///
/// Create a new instance for each query (or keep one around and call
/// `refresh_all()` before reading).
///
/// `sysinfo` returns `Result` from its constructors since 0.40 (it gained an
/// iOS/App Store backend that can legitimately be unsupported), so the
/// monitors are built once here and rebuilt on the first call that finds them
/// missing rather than making every caller handle construction failure.
pub struct SystemMonitor {
    system: Option<System>,
    networks: Option<Networks>,
    disks: Option<Disks>,
}

impl SystemMonitor {
    /// Create a monitor with all information freshly loaded.
    pub fn new() -> Self {
        Self {
            system: System::new_with_specifics(RefreshKind::everything()).ok(),
            networks: Networks::new_with_refreshed_list().ok(),
            disks: Disks::new_with_refreshed_list().ok(),
        }
    }

    /// Refresh and return a full system status snapshot.
    ///
    /// On a platform where `sysinfo` reports no supported backend (iOS), this
    /// returns whatever the static accessors can supply and empty collections
    /// for the rest, rather than failing: `GetSystemStatus` is a read-only
    /// convenience, and a phone has no `ps`/`df` to shell out to instead.
    pub fn get_status(&mut self) -> SystemStatus {
        if self.system.is_none() {
            self.system = System::new_with_specifics(RefreshKind::everything()).ok();
        }
        if self.networks.is_none() {
            self.networks = Networks::new_with_refreshed_list().ok();
        }
        if self.disks.is_none() {
            self.disks = Disks::new_with_refreshed_list().ok();
        }

        if let Some(system) = self.system.as_mut() {
            system.refresh_specifics(RefreshKind::everything());
        }
        if let Some(networks) = self.networks.as_mut() {
            networks.refresh(false);
        }
        if let Some(disks) = self.disks.as_mut() {
            disks.refresh(false);
        }

        let (cpu_usage, cpu_count) = match self.system.as_ref() {
            Some(s) if !s.cpus().is_empty() => {
                let cpus = s.cpus();
                let total: f32 = cpus.iter().map(|c| c.usage()).sum();
                (total / cpus.len() as f32, cpus.len())
            }
            _ => (0.0, 0),
        };

        let disks: Vec<DiskStatus> = self
            .disks
            .as_ref()
            .map(|d| {
                d.iter()
                    .map(|d| {
                        let total = d.total_space();
                        let available = d.available_space();
                        let used = total.saturating_sub(available);
                        DiskStatus {
                            name: d.name().to_string_lossy().to_string(),
                            mount_point: d.mount_point().to_string_lossy().to_string(),
                            total_gb: bytes_to_gb(total),
                            used_gb: bytes_to_gb(used),
                            available_gb: bytes_to_gb(available),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let networks: Vec<NetworkStatus> = self
            .networks
            .as_ref()
            .map(|n| {
                n.iter()
                    .map(|(name, data)| NetworkStatus {
                        name: name.to_string_lossy().to_string(),
                        rx_bytes: data.total_received(),
                        tx_bytes: data.total_transmitted(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        SystemStatus {
            hostname: System::host_name().unwrap_or_default(),
            os_name: System::name().unwrap_or_default(),
            os_version: System::os_version().unwrap_or_default(),
            kernel_version: System::kernel_version().unwrap_or_default(),
            uptime_seconds: System::uptime().unwrap_or_default(),
            cpu_usage_percent: cpu_usage,
            cpu_count,
            memory_total_mb: bytes_to_mb(
                self.system
                    .as_ref()
                    .map(|s| s.total_memory())
                    .unwrap_or_default(),
            ),
            memory_used_mb: bytes_to_mb(
                self.system
                    .as_ref()
                    .map(|s| s.used_memory())
                    .unwrap_or_default(),
            ),
            memory_available_mb: bytes_to_mb(
                self.system
                    .as_ref()
                    .map(|s| s.available_memory())
                    .unwrap_or_default(),
            ),
            swap_total_mb: bytes_to_mb(
                self.system
                    .as_ref()
                    .map(|s| s.total_swap())
                    .unwrap_or_default(),
            ),
            swap_used_mb: bytes_to_mb(
                self.system
                    .as_ref()
                    .map(|s| s.used_swap())
                    .unwrap_or_default(),
            ),
            disks,
            networks,
            timestamp: std::time::Instant::now(),
        }
    }

    /// Refresh and return a list of processes, optionally filtered by name.
    pub fn list_processes(
        &mut self,
        filter: Option<&str>,
        limit: Option<usize>,
    ) -> Vec<ProcessEntry> {
        let Some(system) = self.system.as_mut() else {
            return Vec::new();
        };

        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::everything(),
        );

        let mut entries: Vec<ProcessEntry> = system
            .processes()
            .values()
            .filter_map(|p| {
                let name = p.name().to_string_lossy().to_string();
                if let Some(f) = filter {
                    if !name.to_lowercase().contains(&f.to_lowercase()) {
                        return None;
                    }
                }
                let status = match p.status() {
                    sysinfo::ProcessStatus::Run => "Running",
                    sysinfo::ProcessStatus::Sleep => "Sleeping",
                    sysinfo::ProcessStatus::Stop => "Stopped",
                    sysinfo::ProcessStatus::Idle => "Idle",
                    sysinfo::ProcessStatus::Zombie => "Zombie",
                    _ => "Unknown",
                }
                .to_string();

                Some(ProcessEntry {
                    pid: p.pid().as_u32(),
                    name,
                    cpu_percent: p.cpu_usage(),
                    memory_mb: bytes_to_mb(p.memory()),
                    status,
                    start_time: {
                        let ts = p.start_time() as i64;
                        chrono::DateTime::from_timestamp(ts, 0)
                            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                    },
                })
            })
            .collect();

        // Sort by CPU usage descending
        entries.sort_by(|a, b| {
            b.cpu_percent
                .partial_cmp(&a.cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        if let Some(l) = limit {
            entries.truncate(l);
        }

        entries
    }

    /// Restart a process by PID or name.
    ///
    /// Kills the process (with optional force), then attempts to
    /// re-launch it using the original command line when available.
    /// Returns the new PID if restart succeeds.
    ///
    /// **Blocking:** This method performs a blocking sleep
    /// (`std::thread::sleep`). Call via `tokio::task::spawn_blocking` or
    /// from a blocking context only.
    pub fn restart_process(
        &mut self,
        pid: Option<u32>,
        name: Option<&str>,
        force: bool,
    ) -> Result<u32> {
        let target_pid = self.resolve_pid(pid, name)?;

        // Try to capture the original command line before killing.
        let sys_pid = sysinfo::Pid::from_u32(target_pid);
        let original_cmd = self.system.as_ref().and_then(|system| {
            system
                .process(sys_pid)
                .and_then(|p| p.cmd().first().map(|s| s.to_string_lossy().to_string()))
        });

        // Kill the process.
        self.kill_process(Some(target_pid), None, force)?;

        // Wait briefly for the process to exit.
        std::thread::sleep(Duration::from_millis(500));

        // Restart if we captured the command.
        if let Some(cmd) = original_cmd {
            match std::process::Command::new(&cmd).spawn() {
                Ok(child) => Ok(child.id()),
                Err(e) => {
                    Err(ComputerError::ToolFailed(format!("Failed to restart '{}': {}", cmd, e)))
                }
            }
        } else {
            Err(ComputerError::Other(
                "Could not determine command to restart process".to_string(),
            ))
        }
    }

    /// Set the scheduling priority of a process.
    ///
    /// On Unix-like systems this adjusts the `nice` value via `renice`.
    /// On Windows this sets the priority class via `wmic`.
    ///
    /// `priority` interpretation:
    /// - Unix: nice value (-20 highest to 19 lowest).
    /// - Windows: 0=Idle, 1=BelowNormal, 2=Normal, 3=AboveNormal, 4=High,
    ///   5=Realtime.
    pub fn set_process_priority(
        &mut self,
        pid: Option<u32>,
        name: Option<&str>,
        priority: i32,
    ) -> Result<u32> {
        let target_pid = self.resolve_pid(pid, name)?;

        #[cfg(target_os = "windows")]
        {
            let class = match priority {
                0 => "idle",
                1 => "below normal",
                2 => "normal",
                3 => "above normal",
                4 => "high priority",
                5 => "realtime",
                _ => "normal",
            };
            let script = format!(
                r#"wmic process where ProcessId={} CALL setpriority \"{}\""#,
                target_pid, class
            );
            let output = std::process::Command::new("powershell")
                .args(["-Command", &script])
                .output();
            match output {
                Ok(o) if o.status.success() => Ok(target_pid),
                Ok(o) => Err(ComputerError::ToolFailed(format!(
                    "wmic setpriority failed: {}",
                    String::from_utf8_lossy(&o.stderr)
                ))),
                Err(e) => Err(ComputerError::ToolFailed(format!("Failed to run wmic: {}", e))),
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            let nice = priority.clamp(-20, 19);
            let output = std::process::Command::new("renice")
                .args([&nice.to_string(), "-p", &target_pid.to_string()])
                .output();
            match output {
                Ok(o) if o.status.success() => Ok(target_pid),
                Ok(o) => Err(ComputerError::ToolFailed(format!(
                    "renice failed: {}",
                    String::from_utf8_lossy(&o.stderr)
                ))),
                Err(e) => Err(ComputerError::ToolFailed(format!("Failed to run renice: {}", e))),
            }
        }
    }

    /// Resolve a PID from either an explicit PID or a process name.
    fn resolve_pid(&mut self, pid: Option<u32>, name: Option<&str>) -> Result<u32> {
        if let Some(p) = pid {
            return Ok(p);
        }

        let Some(system) = self.system.as_mut() else {
            return Err(ComputerError::ProcessNotFound(
                "Process inspection is not available on this platform".to_string(),
            ));
        };

        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu(),
        );

        if let Some(n) = name {
            return system
                .processes()
                .values()
                .find(|p| {
                    p.name()
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&n.to_lowercase())
                })
                .map(|p| p.pid().as_u32())
                .ok_or_else(|| {
                    ComputerError::ProcessNotFound(format!("No process matching '{}' found", n))
                });
        }

        Err(ComputerError::ProcessNotFound(
            "Either pid or name must be provided".to_string(),
        ))
    }

    /// Kill a process by PID or name.
    ///
    /// If both `pid` and `name` are provided, PID takes precedence.
    /// If `force` is true, sends SIGKILL (Unix) / force kill (Windows);
    /// otherwise sends SIGTERM (Unix) / normal kill (Windows).
    ///
    /// Returns the PID of the killed process.
    pub fn kill_process(
        &mut self,
        pid: Option<u32>,
        name: Option<&str>,
        force: bool,
    ) -> Result<u32> {
        let target_pid = self.resolve_pid(pid, name)?;

        let sys_pid = sysinfo::Pid::from_u32(target_pid);
        let process = self
            .system
            .as_ref()
            .and_then(|system| system.process(sys_pid))
            .ok_or_else(|| {
                ComputerError::ProcessNotFound(format!("Process {} not found", target_pid))
            })?;

        let signal = if force { Signal::Kill } else { Signal::Term };

        match process.kill_with(signal) {
            Some(true) => Ok(target_pid),
            Some(false) => {
                Err(ComputerError::KillFailed(format!("Failed to kill process {}", target_pid)))
            }
            None => Err(ComputerError::KillFailed(format!(
                "Signal {:?} not supported on this platform",
                signal
            ))),
        }
    }
}

/// Alert triggered when a process exceeds configured thresholds.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessAlert {
    pub pid: u32,
    pub name: String,
    pub cpu_percent: f32,
    pub memory_mb: u64,
    pub message: String,
}

/// Configuration for continuous process monitoring.
#[derive(Debug, Clone)]
pub struct ProcessMonitorConfig {
    /// Poll interval (default: 5 seconds).
    pub poll_interval: Duration,
    /// CPU usage threshold in percent (0 = disabled).
    pub cpu_threshold: f32,
    /// Memory threshold in MB (0 = disabled).
    pub memory_threshold_mb: u64,
    /// Only monitor processes matching these names (empty = all).
    pub filter_names: Vec<String>,
    /// Cooldown between alerts for the same PID (default: 60s).
    pub alert_cooldown: Duration,
}

impl Default for ProcessMonitorConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            cpu_threshold: 80.0,
            memory_threshold_mb: 1024,
            filter_names: vec![],
            alert_cooldown: Duration::from_secs(60),
        }
    }
}

/// Continuous process monitor that polls and emits alerts.
pub struct ProcessMonitor {
    config: ProcessMonitorConfig,
    last_alert: std::collections::HashMap<u32, std::time::Instant>,
    /// Cached system monitor — initialized on first poll to avoid
    /// re-creating a full `RefreshKind::everything` snapshot every cycle.
    monitor: Option<SystemMonitor>,
}

impl ProcessMonitor {
    pub fn new(config: ProcessMonitorConfig) -> Self {
        Self {
            config,
            last_alert: std::collections::HashMap::new(),
            monitor: None,
        }
    }

    /// Poll once and return any alerts for processes exceeding thresholds.
    pub fn poll(&mut self) -> Vec<ProcessAlert> {
        let procs = self
            .monitor
            .get_or_insert_with(SystemMonitor::new)
            .list_processes(None, None);
        let mut alerts = Vec::new();
        let now = std::time::Instant::now();

        for p in procs {
            // Filter by name if configured
            if !self.config.filter_names.is_empty() {
                let matches = self
                    .config
                    .filter_names
                    .iter()
                    .any(|f| p.name.to_lowercase().contains(&f.to_lowercase()));
                if !matches {
                    continue;
                }
            }

            let mut triggered = false;
            let mut reasons = Vec::new();

            if self.config.cpu_threshold > 0.0 && p.cpu_percent >= self.config.cpu_threshold {
                triggered = true;
                reasons.push(format!("CPU {:.1}%", p.cpu_percent));
            }

            if self.config.memory_threshold_mb > 0 && p.memory_mb >= self.config.memory_threshold_mb
            {
                triggered = true;
                reasons.push(format!("memory {} MB", p.memory_mb));
            }

            if triggered {
                // Check cooldown
                if let Some(last) = self.last_alert.get(&p.pid) {
                    if now.duration_since(*last) < self.config.alert_cooldown {
                        continue;
                    }
                }
                self.last_alert.insert(p.pid, now);
                let name = p.name.clone();
                alerts.push(ProcessAlert {
                    pid: p.pid,
                    name,
                    cpu_percent: p.cpu_percent,
                    memory_mb: p.memory_mb,
                    message: format!(
                        "Process {} (PID {}) exceeded thresholds: {}",
                        p.name,
                        p.pid,
                        reasons.join(", ")
                    ),
                });
            }
        }

        alerts
    }

    /// Run a single monitoring cycle and log alerts via `tracing`.
    pub fn check_and_log(&mut self) {
        for alert in self.poll() {
            tracing::warn!(
                "Process alert: {} (CPU {:.1}%, Memory {} MB)",
                alert.message,
                alert.cpu_percent,
                alert.memory_mb
            );
        }
    }
}

impl Default for SystemMonitor {
    fn default() -> Self {
        Self::new()
    }
}

fn bytes_to_mb(bytes: u64) -> u64 {
    bytes / 1_024 / 1_024
}

fn bytes_to_gb(bytes: u64) -> f64 {
    bytes as f64 / 1_024.0 / 1_024.0 / 1_024.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_monitor_creation() {
        let monitor = SystemMonitor::new();
        // Just verify it doesn't panic
        let _ = monitor;
    }

    #[test]
    fn test_get_status() {
        let mut monitor = SystemMonitor::new();
        let status = monitor.get_status();
        assert!(!status.hostname.is_empty());
        assert!(status.cpu_count > 0);
        assert!(status.memory_total_mb > 0);
    }

    #[test]
    fn test_list_processes() {
        let mut monitor = SystemMonitor::new();
        let procs = monitor.list_processes(None, None);
        assert!(!procs.is_empty());

        // Current process must be in the list
        let current_pid = std::process::id();
        assert!(procs.iter().any(|p| p.pid == current_pid));
    }

    #[test]
    fn test_list_processes_filter() {
        let mut monitor = SystemMonitor::new();
        let all = monitor.list_processes(None, Some(10));
        assert!(!all.is_empty());

        // Filter by a name that definitely doesn't exist
        let filtered = monitor.list_processes(Some("xyzzy_nonexistent_12345"), None);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_list_processes_limit() {
        let mut monitor = SystemMonitor::new();
        let procs = monitor.list_processes(None, Some(5));
        assert!(procs.len() <= 5);
    }

    #[test]
    fn test_kill_process_by_name_not_found() {
        let mut monitor = SystemMonitor::new();
        let result = monitor.kill_process(None, Some("xyzzy_nonexistent_12345"), false);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_kill_process_spawn_and_kill() {
        // Spawn a sleep process
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("failed to spawn sleep");

        let pid = child.id().unwrap();

        // Small delay to let the process appear
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let mut monitor = SystemMonitor::new();
        let result = monitor.kill_process(Some(pid), None, false);
        assert!(result.is_ok(), "kill failed: {:?}", result);
        assert_eq!(result.unwrap(), pid);

        // Wait for the child to actually exit
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
    }

    #[test]
    fn test_process_monitor_config_default() {
        let cfg = ProcessMonitorConfig::default();
        assert_eq!(cfg.poll_interval, Duration::from_secs(5));
        assert_eq!(cfg.cpu_threshold, 80.0);
        assert_eq!(cfg.memory_threshold_mb, 1024);
        assert!(cfg.filter_names.is_empty());
    }

    #[test]
    fn test_process_monitor_poll_no_alerts_when_idle() {
        // Use very high thresholds so nothing triggers
        let cfg = ProcessMonitorConfig {
            poll_interval: Duration::from_secs(1),
            cpu_threshold: 999.0,
            memory_threshold_mb: 999_999,
            filter_names: vec![],
            alert_cooldown: Duration::from_secs(0),
        };
        let mut monitor = ProcessMonitor::new(cfg);
        let alerts = monitor.poll();
        assert!(alerts.is_empty());
    }

    #[test]
    fn test_process_monitor_filter_by_name() {
        let cfg = ProcessMonitorConfig {
            poll_interval: Duration::from_secs(1),
            cpu_threshold: 0.0,     // disabled
            memory_threshold_mb: 0, // disabled
            filter_names: vec!["xyzzy_nonexistent_99999".to_string()],
            alert_cooldown: Duration::from_secs(0),
        };
        let mut monitor = ProcessMonitor::new(cfg);
        let alerts = monitor.poll();
        assert!(alerts.is_empty());
    }

    #[test]
    fn test_process_alert_display() {
        let alert = ProcessAlert {
            pid: 1234,
            name: "test".to_string(),
            cpu_percent: 95.5,
            memory_mb: 2048,
            message: "test alert".to_string(),
        };
        assert!(alert.message.contains("test alert"));
        assert_eq!(alert.pid, 1234);
    }
}
