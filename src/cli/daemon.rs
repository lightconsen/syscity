//! Daemon management commands for Syscity

use std::path::PathBuf;

use crate::daemon::{DaemonConfig, DaemonManager};
use crate::error::Result;

/// Run health check
pub async fn run_health_check(_config: &crate::config::Config) -> Result<()> {
    println!("🏥 Health Check");
    println!("===============");

    // Check config
    println!("✅ Configuration loaded");

    // Check daemon status
    let daemon_config = DaemonConfig {
        host: "127.0.0.1".to_string(),
        port: 18080,
        pid_file: crate::dirs::syscity_dir().join("syscity.pid"),
        remote_control_host: None,
        remote_control_user: None,
        remote_control_port: 0,
        remote_control_protocol: "ssh".to_string(),
        remote_control_key: None,
        headless: false,
        headless_display: String::new(),
        nocloud: false,
    };
    let daemon = DaemonManager::new(daemon_config)?;
    daemon.status().await?;

    Ok(())
}

/// Run as an assistant process
///
/// Reads messages from stdin (one per line), sends each to the daemon's
/// `/api/chat` endpoint, and writes the response to stdout.  Designed for
/// use in shell pipelines and editor integrations.
pub async fn run_assistant_process(_config_path: &PathBuf) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    const DAEMON_URL: &str = "http://127.0.0.1:18080";
    let client = reqwest::Client::new();
    let session_id = uuid::Uuid::new_v4().to_string();

    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let url = format!("{}/api/chat", DAEMON_URL);
        let body = serde_json::json!({
            "session_id": session_id,
            "message": line,
        });

        match client.post(&url).json(&body).send().await {
            Ok(resp) => {
                let text = resp.text().await.unwrap_or_default();
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    let content = json
                        .get("response")
                        .or_else(|| json.get("content"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(&text);
                    println!("{}", content);
                } else {
                    println!("{}", text);
                }
            }
            Err(e) => {
                eprintln!("Daemon error: {}", e);
                eprintln!("Is the daemon running? Try: syscity start");
                return Err(crate::error::SyscityError::Internal(e.to_string()));
            }
        }
    }

    Ok(())
}

/// Start the daemon
pub async fn run_start_daemon(
    foreground: bool,
    _config: &crate::config::Config,
    daemon_config: DaemonConfig,
) -> Result<()> {
    let daemon_config = DaemonConfig {
        pid_file: crate::dirs::syscity_dir().join("syscity.pid"),
        ..daemon_config
    };

    let daemon = DaemonManager::new(daemon_config)?;

    if foreground {
        // Run in foreground with Gateway
        daemon.run_foreground().await
    } else {
        // Start in background
        daemon.start().await
    }
}

/// Reload plugins, configuration, providers, MCP servers, and skills
/// without restarting the daemon (WS `system.reload`).
pub async fn run_reload_daemon() -> Result<()> {
    match crate::cli::ws::call("system.reload", serde_json::json!({ "scope": "all" })).await {
        Ok(json) => {
            println!("Daemon reloaded successfully.");
            if let Some(plugins) = json.get("plugins") {
                let unloaded = plugins["unloaded"].as_u64().unwrap_or(0);
                let loaded = plugins["loaded"].as_u64().unwrap_or(0);
                println!("  Plugins:  unloaded={}, loaded={}", unloaded, loaded);
            }
            if let Some(cfg) = json.get("config") {
                if cfg["updated"].as_bool().unwrap_or(false) {
                    println!("  Config:   updated");
                } else {
                    println!("  Config:   no change");
                }
            }
            if let Some(providers) = json.get("providers") {
                let added = providers["added"].as_u64().unwrap_or(0);
                let removed = providers["removed"].as_u64().unwrap_or(0);
                println!("  Providers: added={}, removed={}", added, removed);
            }
            if let Some(mcp) = json.get("mcp") {
                let connected = mcp["connected"].as_u64().unwrap_or(0);
                let failed = mcp["failed"].as_u64().unwrap_or(0);
                println!("  MCP:      connected={}, failed={}", connected, failed);
            }
            if let Some(skills) = json.get("skills") {
                if skills["reinitialized"].as_bool().unwrap_or(false) {
                    let count = skills["count"].as_u64().unwrap_or(0);
                    println!("  Skills:   reinitialized ({} skills)", count);
                } else {
                    println!("  Skills:   reinitialization failed");
                }
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("Reload failed: {}", e);
            eprintln!("Is the daemon running? Try: syscity start");
            Err(e)
        }
    }
}

/// Stop the daemon
pub async fn run_stop_daemon(force: bool) -> Result<()> {
    let daemon_config = DaemonConfig {
        host: "127.0.0.1".to_string(),
        port: 18080,
        pid_file: crate::dirs::syscity_dir().join("syscity.pid"),
        remote_control_host: None,
        remote_control_user: None,
        remote_control_port: 0,
        remote_control_protocol: "ssh".to_string(),
        remote_control_key: None,
        headless: false,
        headless_display: String::new(),
        nocloud: false,
    };

    let daemon = DaemonManager::new(daemon_config)?;

    if force {
        daemon.stop_force().await
    } else {
        daemon.stop().await
    }
}

/// Restart the daemon after a self-update, waiting for the old process to
/// exit first.
///
/// The old daemon spawns this helper (via `syscity restart --pid <self>`)
/// after atomically replacing its own binary, then exits. This helper waits
/// for the recorded PID to disappear so the port is freed, then starts a fresh
/// daemon — which is the newly installed binary. Used only by the web/daemon
/// update flow.
pub async fn run_restart_daemon(
    pid: Option<u32>,
    host: &str,
    port: u16,
    nocloud: bool,
) -> Result<()> {
    if let Some(pid) = pid {
        let daemon = DaemonManager::new(DaemonConfig {
            host: host.to_string(),
            port,
            pid_file: crate::dirs::syscity_dir().join("syscity.pid"),
            remote_control_host: None,
            remote_control_user: None,
            remote_control_port: 0,
            remote_control_protocol: "ssh".to_string(),
            remote_control_key: None,
            headless: false,
            headless_display: String::new(),
            nocloud,
        })?;

        // Wait up to 15s for the old daemon to fully exit so the port is free
        // before starting the replacement.
        for _ in 0..150 {
            if !daemon.is_process_running(pid).await {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }

    let daemon = DaemonManager::new(DaemonConfig {
        host: host.to_string(),
        port,
        pid_file: crate::dirs::syscity_dir().join("syscity.pid"),
        remote_control_host: None,
        remote_control_user: None,
        remote_control_port: 0,
        remote_control_protocol: "ssh".to_string(),
        remote_control_key: None,
        headless: false,
        headless_display: String::new(),
        nocloud,
    })?;
    daemon.start().await
}

/// Check daemon status
pub async fn run_daemon_status() -> Result<()> {
    // Report the endpoint the CLI would actually talk to, not a hardcoded
    // default — a daemon started with `--port` must not be described as
    // listening on 18080.
    let (host, port) = super::ws::endpoint();
    let daemon_config = DaemonConfig {
        host,
        port,
        pid_file: crate::dirs::syscity_dir().join("syscity.pid"),
        remote_control_host: None,
        remote_control_user: None,
        remote_control_port: 0,
        remote_control_protocol: "ssh".to_string(),
        remote_control_key: None,
        headless: false,
        headless_display: String::new(),
        nocloud: false,
    };

    let daemon = DaemonManager::new(daemon_config)?;
    daemon.status().await
}

/// Show and tail daemon logs
pub async fn run_logs(lines: usize, follow: bool) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::time::{interval, Duration};

    let log_path = crate::logs::log_file_path();

    if !log_path.exists() {
        println!("No log file found at {:?}", log_path);
        return Ok(());
    }

    println!("📋 Logs from: {:?}", log_path);
    println!();

    if follow {
        // Tail mode - read last N lines then follow
        let file = tokio::fs::File::open(&log_path).await?;
        let reader = BufReader::new(file);
        let mut lines_stream = reader.lines();

        // Collect and show last N lines
        let mut all_lines: Vec<String> = Vec::new();
        while let Ok(Some(line)) = lines_stream.next_line().await {
            all_lines.push(line);
            if all_lines.len() > lines {
                all_lines.remove(0);
            }
        }

        for line in all_lines {
            println!("{}", line);
        }

        // Continue following
        println!("\n--- Following log (Ctrl+C to exit) ---\n");

        let mut interval = interval(Duration::from_millis(500));
        let mut last_pos = tokio::fs::metadata(&log_path).await?.len();

        loop {
            interval.tick().await;

            let metadata = tokio::fs::metadata(&log_path).await?;
            let new_len = metadata.len();

            if new_len > last_pos {
                let file = tokio::fs::File::open(&log_path).await?;
                let reader = BufReader::new(file);
                let mut lines_stream = reader.lines();

                // Skip to last known position
                let mut pos = 0u64;
                while pos < last_pos {
                    if let Ok(Some(line)) = lines_stream.next_line().await {
                        pos += line.len() as u64 + 1; // +1 for newline
                    } else {
                        break;
                    }
                }

                // Print new lines
                while let Ok(Some(line)) = lines_stream.next_line().await {
                    println!("{}", line);
                }

                last_pos = new_len;
            }
        }
    } else {
        // Just show last N lines
        let file = tokio::fs::File::open(&log_path).await?;
        let reader = BufReader::new(file);
        let mut lines_stream = reader.lines();

        let mut all_lines: Vec<String> = Vec::new();
        while let Ok(Some(line)) = lines_stream.next_line().await {
            all_lines.push(line);
            if all_lines.len() > lines {
                all_lines.remove(0);
            }
        }

        for line in all_lines {
            println!("{}", line);
        }

        println!("\n--- Use -f to follow logs ---");
    }

    Ok(())
}
