//! Linux implementation of [`SystemInspector`].
//!
//! Named for the trait it implements, not for the module it feeds — the two
//! used to share the filename `server_operator.rs`, which read as though the
//! platform layer and the Linux layer were the same thing.
//!
//! **Not wired up yet.** Nothing constructs this inspector: there is no tool
//! registered against it and no caller of [`ServerOperator`]. It is the
//! platform half of a layer designed over the live `SystemInspectTool`; treat
//! it as a seed rather than something a running gateway uses.
//!
//! [`SystemInspector`]: crate::computer::platform::server_operator::SystemInspector
//! [`ServerOperator`]: crate::computer::platform::server_operator::ServerOperator

use crate::computer::platform::linux::system_inspect::SystemInspectTool;
use crate::computer::platform::server_operator::{SystemInspector, SystemSnapshot};

/// Linux-specific system inspector.
#[derive(Debug, Default)]
pub struct LinuxSystemInspector;

impl LinuxSystemInspector {
    /// Create a new inspector.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl SystemInspector for LinuxSystemInspector {
    async fn inspect_full(&self) -> crate::Result<SystemSnapshot> {
        let (hostname, uptime, load_avg, memory, cpu_count) =
            SystemInspectTool::collect_overview().await;

        let (disks, processes, services, listening_ports, recent_logs) = tokio::join!(
            SystemInspectTool::collect_storage(),
            SystemInspectTool::collect_processes(20),
            SystemInspectTool::collect_services(20),
            SystemInspectTool::collect_network(),
            SystemInspectTool::collect_logs(30, "1 hour ago"),
        );

        Ok(SystemSnapshot {
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
        })
    }
}
