//! Daemon management for Syscity
//!
//! Provides start/stop/status functionality for running Syscity as a background
//! service.

// INVARIANTS-NONE: process launcher (start/stop); the runtime holds its state
use std::path::PathBuf;

use tokio::process::Command;
use tracing::{info, warn};

/// Daemon configuration
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Host to bind to
    pub host: String,
    /// Port for gateway API, WebSocket, and SPA
    pub port: u16,
    /// Path to PID file
    pub pid_file: PathBuf,
    /// Remote control target host (CLI override)
    pub remote_control_host: Option<String>,
    /// Remote control username (CLI override)
    pub remote_control_user: Option<String>,
    /// Remote control port (CLI override)
    pub remote_control_port: u16,
    /// Remote control protocol (CLI override)
    pub remote_control_protocol: String,
    /// Remote control SSH key path (CLI override)
    pub remote_control_key: Option<String>,
    /// Enable headless mode (CLI override)
    pub headless: bool,
    /// Headless display identifier (CLI override)
    pub headless_display: String,
    /// Force-disable cloud features (CLI override; cloud is on by default)
    pub nocloud: bool,
}

/// The `config.toml` written on first start.
///
/// Every key here is one `GatewayConfig` actually reads, and the top-level
/// keys come first because TOML attaches a bare key to the table above it. The
/// file used to disagree with the schema in four places — `[model]` and
/// `[server]` as tables over scalar fields (which made the whole document fail
/// to parse, so the daemon silently ran on defaults), `connection` (the field
/// is `database_url`), and `workspace_only` sitting inside `[cost_guard]` —
/// and `#[serde(default)]` on the structs means a key the schema does not know
/// is dropped without a word. `default_config_toml_round_trips` is what keeps
/// that from happening again.
pub const DEFAULT_CONFIG_TOML: &str = r#"# Syscity Configuration
# Auto-generated on first start

# ── Server ────────────────────────────────────────────────────────────
host = "127.0.0.1"
port = 18080

# ── Model ─────────────────────────────────────────────────────────────
# Uncomment to pin one. Left out on purpose: an unconfigured gateway uses the
# built-in default (`providers::DEFAULT_MODEL`), so this file is not a second
# place that has to be kept in step with it.
# model = "claude-sonnet-5"
# model_provider = "anthropic"

# ── Workspace ─────────────────────────────────────────────────────────
# Restrict file operations to this directory. Without `workspace_dir` the
# default is ~/.syscity/workspace.
# workspace_dir = "~/projects"
workspace_only = true

[security]
enabled = true
auth_required = false
pairing_required = false
auth_mode = "none"
shared_token = ""
security_headers = true
# Credential precedence: "env_first" (env overrides config) or "config_first" (config overrides env)
# credential_precedence = "env_first"

[security.rate_limit]
enabled = true
capacity = 100
refill_rate = 10

[storage]
storage_type = "sqlite"
# database_url = "sqlite:///path/to/syscity.db"

[acp]
enabled = true
max_subagents = 10
default_timeout_seconds = 300

[cron]
enabled = true
check_interval_seconds = 60

[plugins]
enabled = true
auto_load = true

[hot_reload]
enabled = true
watch_config = true
watch_agents = true
watch_plugins = true
debounce_seconds = 2

[cost_guard]
daily_limit_cents = 0
hourly_action_limit = 0
"#;

/// A `config.toml` template that does not parse is worse than none: the daemon
/// warns once and runs on defaults, so every setting in the file looks applied
/// and is not.
#[cfg(test)]
mod default_config_tests {
    use super::DEFAULT_CONFIG_TOML;
    use crate::gateway::GatewayConfig;

    #[test]
    fn default_config_toml_round_trips() {
        let config: GatewayConfig = toml::from_str(DEFAULT_CONFIG_TOML)
            .expect("the generated config.toml must parse as a GatewayConfig");

        // Parsing is not enough — the point is that these values take effect
        // rather than landing in a table the schema never reads.
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 18080);
        // The template deliberately does not pin a model: the built-in default
        // applies until the user writes one, and that default lives in exactly
        // one place (`providers::DEFAULT_MODEL`).
        assert_eq!(config.model, crate::providers::DEFAULT_MODEL);
        assert_eq!(config.model_provider, crate::providers::DEFAULT_MODEL_PROVIDER);
        assert!(config.workspace_only);
        assert_eq!(config.storage.storage_type, "sqlite");
        assert!(config.security.enabled);
        assert_eq!(config.security.rate_limit.capacity, 100);
        assert!(config.cron.enabled);
        assert!(config.plugins.auto_load);
        assert_eq!(config.hot_reload.debounce_seconds, 2);

        // `workspace_only` defaults to true, so asserting it here would pass
        // even if the key were landing in a table nothing reads (which it was:
        // it sat under `[cost_guard]`). Flipping it in the template has to
        // flip it here.
        let flipped =
            DEFAULT_CONFIG_TOML.replace("workspace_only = true", "workspace_only = false");
        let config: GatewayConfig = toml::from_str(&flipped).unwrap();
        assert!(!config.workspace_only, "workspace_only must be read from the top level");
    }
}

/// Daemon manager
pub struct DaemonManager {
    config: DaemonConfig,
}

impl DaemonManager {
    /// Create a new daemon manager
    pub fn new(config: DaemonConfig) -> crate::Result<Self> {
        Ok(Self { config })
    }

    /// Start the daemon in the background
    pub async fn start(&self) -> crate::Result<()> {
        // Check if already running
        if let Some(pid) = self.read_pid().await {
            if self.is_process_running(pid).await {
                println!("✅ Syscity daemon is already running (PID: {})", pid);
                return Ok(());
            }
            // Stale PID file, remove it
            let _ = tokio::fs::remove_file(&self.config.pid_file).await;
        }

        // Get the current executable path
        let exe_path = std::env::current_exe().map_err(crate::error::SyscityError::Io)?;

        // Get log file path
        let log_path = crate::logs::log_file_path();

        // Ensure log directory exists
        if let Some(parent) = log_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Open log file for appending (std version for process spawning)
        let log_file_std = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(crate::error::SyscityError::Io)?;

        // Spawn the daemon process with output redirected to log file
        let mut cmd = Command::new(&exe_path);
        cmd.arg("start")
            .arg("--host")
            .arg(&self.config.host)
            .arg("--port")
            .arg(self.config.port.to_string())
            .arg("--foreground");
        if self.config.nocloud {
            // Propagate the cloud opt-out to the detached child; without this
            // a backgrounded `syscity start --nocloud` would silently lose it.
            cmd.arg("--nocloud");
        }
        let child = cmd
            .stdin(std::process::Stdio::null())
            .stdout(
                log_file_std
                    .try_clone()
                    .map_err(crate::error::SyscityError::Io)?,
            )
            .stderr(log_file_std)
            .spawn()
            .map_err(crate::error::SyscityError::Io)?;

        let pid = child.id().ok_or_else(|| {
            crate::error::SyscityError::Internal("Failed to get child PID".to_string())
        })?;

        // Write PID file
        self.write_pid(pid).await?;

        println!("✅ Syscity daemon started (PID: {})", pid);
        println!("   Host: {}", self.config.host);
        println!("   Port: {}", self.config.port);
        println!("   URL: http://{}:{}", self.config.host, self.config.port);
        println!("   Logs: {:?}", log_path);

        Ok(())
    }

    /// Check whether the daemon is currently running.
    ///
    /// Returns `false` when there is no PID file or the recorded process is
    /// gone (a stale PID file). Used by the updater to decide whether to
    /// restart the daemon after installing a new binary.
    pub async fn is_running(&self) -> crate::Result<bool> {
        match self.read_pid().await {
            Some(pid) => Ok(self.is_process_running(pid).await),
            None => Ok(false),
        }
    }

    /// Spawn a detached helper that restarts the daemon after the running
    /// binary has been replaced by the self-updater.
    ///
    /// The helper is this same executable invoked as `restart --pid <self>`
    /// with the old process's PID. It waits for the old daemon to exit (freeing
    /// the port), then starts a fresh daemon — which, since `current_exe` was
    /// atomically renamed, is the newly installed binary. Returns immediately;
    /// the caller is expected to shut down promptly so the helper can take
    /// over. This is only used from the web/daemon update flow, where the
    /// process performing the update is the process that must be replaced.
    ///
    /// `nocloud` preserves a CLI `--nocloud` override across the restart: the
    /// flag lives only on the command line, never in config.toml.
    pub fn spawn_restart_helper(
        host: &str,
        port: u16,
        old_pid: u32,
        nocloud: bool,
    ) -> crate::Result<()> {
        let exe_path = std::env::current_exe().map_err(crate::error::SyscityError::Io)?;
        let log_path = crate::logs::log_file_path();
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent).map_err(crate::error::SyscityError::Io)?;
        }
        let log_file_std = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(crate::error::SyscityError::Io)?;

        let mut cmd = std::process::Command::new(&exe_path);
        cmd.arg("restart")
            .arg("--pid")
            .arg(old_pid.to_string())
            .arg("--host")
            .arg(host)
            .arg("--port")
            .arg(port.to_string());
        if nocloud {
            cmd.arg("--nocloud");
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(
                log_file_std
                    .try_clone()
                    .map_err(crate::error::SyscityError::Io)?,
            )
            .stderr(log_file_std)
            .spawn()
            .map_err(crate::error::SyscityError::Io)?;

        Ok(())
    }
}

/// Apply environment variable overrides for security credentials,
/// respecting `security.credential_precedence`.
fn apply_env_security_overrides(config: &mut crate::gateway::GatewayConfig) {
    use crate::gateway::CredentialPrecedence;

    let precedence = config.security.credential_precedence;

    if let Ok(token) = std::env::var("SYSCITY_SECURITY_SHARED_TOKEN") {
        let config_empty = config
            .security
            .shared_token
            .as_ref()
            .map(|s| s.is_empty())
            .unwrap_or(true);

        match precedence {
            CredentialPrecedence::EnvFirst => {
                config.security.shared_token = Some(token);
            }
            CredentialPrecedence::ConfigFirst if config_empty => {
                config.security.shared_token = Some(token);
            }
            CredentialPrecedence::ConfigFirst => {}
        }
    }

    // Enable an auth mode via env. config.toml is not reliably parsed by the
    // gateway (the [server]/[model] template does not match GatewayConfig's
    // top-level fields — see the config-parse bug), so the env path is the
    // dependable way to secure a remote listener.
    if let Ok(mode) = std::env::var("SYSCITY_SECURITY_AUTH_MODE") {
        let parsed = match mode.trim().to_ascii_lowercase().as_str() {
            "none" => Some(crate::gateway::protocol::AuthMode::None),
            "token" => Some(crate::gateway::protocol::AuthMode::Token),
            "device" => Some(crate::gateway::protocol::AuthMode::Device),
            "tailscale" => Some(crate::gateway::protocol::AuthMode::Tailscale),
            _ => {
                tracing::warn!("Ignoring unknown SYSCITY_SECURITY_AUTH_MODE value: {}", mode);
                None
            }
        };
        if let Some(mode) = parsed {
            config.security.auth_mode = mode;
        }
    }
    if let Ok(v) = std::env::var("SYSCITY_SECURITY_AUTH_REQUIRED") {
        match v.parse::<bool>() {
            Ok(b) => config.security.auth_required = b,
            Err(_) => {
                tracing::warn!("Ignoring invalid SYSCITY_SECURITY_AUTH_REQUIRED value: {}", v)
            }
        }
    }
}

/// Apply environment variable overrides for LLM provider credentials,
/// respecting `security.credential_precedence`.
fn apply_env_provider_overrides(config: &mut crate::gateway::GatewayConfig) {
    use crate::gateway::CredentialPrecedence;

    let precedence = config.security.credential_precedence;

    // SYSCITY_BASE_URL + SYSCITY_API_KEY pair creates a provider.
    if let (Ok(base_url), Ok(api_key)) =
        (std::env::var("SYSCITY_BASE_URL"), std::env::var("SYSCITY_API_KEY"))
    {
        let is_anthropic = std::env::var("SYSCITY_IS_ANTHROPIC")
            .map(|v| v.to_lowercase() == "true" || v == "1")
            .unwrap_or(false);

        let provider_type = if is_anthropic {
            crate::model_router::ProviderType::Anthropic
        } else {
            crate::model_router::ProviderType::OpenAi
        };

        let provider_name = std::env::var("SYSCITY_MODEL_PROVIDER").unwrap_or_else(|_| {
            if is_anthropic {
                "anthropic".to_string()
            } else {
                "openai".to_string()
            }
        });

        let should_insert = match precedence {
            CredentialPrecedence::EnvFirst => true,
            CredentialPrecedence::ConfigFirst => !config.providers.contains_key(&provider_name),
        };

        if should_insert {
            let provider_config = crate::model_router::ProviderConfig {
                provider_type,
                models: Vec::new(),
                default_model: String::new(),
                api_key: api_key.into(),
                api_keys: Vec::new(),
                auth_profile: None,
                oauth: None,
                base_url: Some(base_url),
                timeout: std::time::Duration::from_secs(60),
                max_retries: 3,
                retry_delay_ms: 1000,
            };
            config
                .providers
                .insert(provider_name.clone(), provider_config);
            println!("🤖 Configured {} provider from environment", provider_name);
        }
    } else if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        let should_insert = match precedence {
            CredentialPrecedence::EnvFirst => true,
            CredentialPrecedence::ConfigFirst => !config.providers.contains_key("anthropic"),
        };

        if should_insert {
            let provider_config = crate::model_router::ProviderConfig {
                provider_type: crate::model_router::ProviderType::Anthropic,
                models: Vec::new(),
                default_model: String::new(),
                api_key: api_key.into(),
                api_keys: Vec::new(),
                auth_profile: None,
                oauth: None,
                base_url: None,
                timeout: std::time::Duration::from_secs(60),
                max_retries: 3,
                retry_delay_ms: 1000,
            };
            config
                .providers
                .insert("anthropic".to_string(), provider_config);
            println!("🤖 Configured Anthropic provider from ANTHROPIC_API_KEY");
        }
    } else if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        let should_insert = match precedence {
            CredentialPrecedence::EnvFirst => true,
            CredentialPrecedence::ConfigFirst => !config.providers.contains_key("openai"),
        };

        if should_insert {
            let provider_config = crate::model_router::ProviderConfig {
                provider_type: crate::model_router::ProviderType::OpenAi,
                models: Vec::new(),
                default_model: String::new(),
                api_key: api_key.into(),
                api_keys: Vec::new(),
                auth_profile: None,
                oauth: None,
                base_url: None,
                timeout: std::time::Duration::from_secs(60),
                max_retries: 3,
                retry_delay_ms: 1000,
            };
            config
                .providers
                .insert("openai".to_string(), provider_config);
            println!("🤖 Configured OpenAI provider from OPENAI_API_KEY");
        }
    }
}

impl DaemonManager {
    pub async fn run_foreground(&self) -> crate::Result<()> {
        println!("🚀 Syscity daemon running with Gateway...");
        println!("   Version: {} (build: {})", crate::VERSION, crate::GIT_HASH);

        use crate::gateway::{Gateway, GatewayConfig};

        // ── Auto-initialize ~/.syscity directory and config.toml ──────────────
        let syscity_dir = crate::dirs::syscity_dir();
        let config_path = crate::dirs::default_config_file();

        if !syscity_dir.exists() {
            println!("📁 Creating Syscity directory at {:?}...", syscity_dir);
            tokio::fs::create_dir_all(&syscity_dir)
                .await
                .map_err(crate::error::SyscityError::Io)?;
        }

        if !config_path.exists() {
            println!("📄 Creating default config.toml at {:?}...", config_path);
            tokio::fs::write(&config_path, DEFAULT_CONFIG_TOML)
                .await
                .map_err(crate::error::SyscityError::Io)?;
            println!("✅ Default config created. Edit {:?} to customize.", config_path);
        }

        // Try to load existing Gateway config from config.toml
        let mut gateway_config = if config_path.exists() {
            match tokio::fs::read_to_string(&config_path).await {
                Ok(content) => {
                    match toml::from_str::<GatewayConfig>(&content) {
                        Ok(mut config) => {
                            // Override with daemon config for host/port
                            config.host = self.config.host.clone();
                            config.port = self.config.port;
                            println!("📄 Loaded Gateway config from {:?}", config_path);
                            println!("   Channels configured: {}", config.channels.len());
                            for (name, ch) in &config.channels {
                                println!("   - {}: enabled={}", name, ch.enabled);
                            }
                            config
                        }
                        Err(e) => {
                            // Spell out the consequence: a config that does not
                            // parse is not "mostly applied" — nothing in it is.
                            warn!(
                                "Failed to parse {:?}: {}. No setting from that file is in \
                                 effect; running on defaults.",
                                config_path, e
                            );
                            let mut default_config = GatewayConfig::default();
                            // Attempt to extract [search] section separately, since the
                            // config.toml uses [server] section format that doesn't
                            // match GatewayConfig's flat host/port fields.
                            if let Ok(toml_value) = content.parse::<toml::Value>() {
                                if let Some(search_section) = toml_value.get("search") {
                                    let search_toml =
                                        toml::to_string(search_section).unwrap_or_default();
                                    if let Ok(search) =
                                        toml::from_str::<crate::gateway::config::SearchConfig>(
                                            &search_toml,
                                        )
                                    {
                                        default_config.search = search;
                                        info!(
                                            "Extracted search config from config.toml: {:?}",
                                            default_config.search.provider_list()
                                        );
                                    }
                                }
                            }
                            default_config
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to read config.toml: {}, using defaults", e);
                    GatewayConfig::default()
                }
            }
        } else {
            println!("📄 No config.toml found, using default config");
            GatewayConfig::default()
        };

        // Apply environment overrides
        gateway_config.host = self.config.host.clone();
        gateway_config.port = self.config.port;
        gateway_config.model =
            std::env::var("SYSCITY_MODEL").unwrap_or_else(|_| gateway_config.model.clone());
        gateway_config.model_provider = std::env::var("SYSCITY_MODEL_PROVIDER")
            .unwrap_or_else(|_| gateway_config.model_provider.clone());

        // Apply credential precedence for security tokens
        apply_env_security_overrides(&mut gateway_config);

        // Discourage plaintext shared_token in config.toml — the env reference
        // is preferred so the value never lands in the on-disk config.
        if std::env::var("SYSCITY_SECURITY_SHARED_TOKEN").is_err()
            && gateway_config
                .security
                .shared_token
                .as_ref()
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        {
            warn!(
                "security.shared_token is set in plaintext config; \
                 prefer the SYSCITY_SECURITY_SHARED_TOKEN environment variable"
            );
        }

        // Enable features based on environment variables
        // Vector Memory - enabled by default with local GGUF embeddings
        if std::env::var("SYSCITY_VECTOR_MEMORY_ENABLED")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false)
        {
            gateway_config.vector_memory.enabled = true;
            gateway_config.vector_memory.embedding_api_key =
                std::env::var("SYSCITY_EMBEDDING_API_KEY").ok();
            if let Ok(model) = std::env::var("SYSCITY_EMBEDDING_MODEL") {
                gateway_config.vector_memory.embedding_model = model;
            }
            println!("📊 Vector memory enabled");
        }

        // Plugins - enabled by default, disable if explicitly set to false
        if std::env::var("SYSCITY_PLUGINS_ENABLED")
            .map(|v| v == "false" || v == "0")
            .unwrap_or(false)
        {
            gateway_config.plugins.enabled = false;
            println!("🔌 Plugins disabled via environment");
        } else {
            println!("🔌 Plugins enabled (auto-load: {})", gateway_config.plugins.auto_load);
        }

        // Hot Reload - enabled by default, disable if explicitly set to false
        if std::env::var("SYSCITY_HOT_RELOAD_ENABLED")
            .map(|v| v == "false" || v == "0")
            .unwrap_or(false)
        {
            gateway_config.hot_reload.enabled = false;
            println!("♻️  Hot reload disabled via environment");
        } else {
            println!("♻️  Hot reload enabled");
        }

        // ACP - enabled by default, disable if explicitly set to false
        if std::env::var("SYSCITY_ACP_ENABLED")
            .map(|v| v == "false" || v == "0")
            .unwrap_or(false)
        {
            gateway_config.acp.enabled = false;
            println!("🎛️  ACP disabled via environment");
        } else {
            println!("🎛️  ACP enabled");
        }

        // Apply CLI overrides for computer / remote control config
        if let Some(ref host) = self.config.remote_control_host {
            gateway_config.computer.remote_control.host = Some(host.clone());
            println!("🖥️  Remote control host: {}", host);
        }
        if let Some(ref user) = self.config.remote_control_user {
            gateway_config.computer.remote_control.user = Some(user.clone());
        }
        if self.config.remote_control_port != 22 {
            gateway_config.computer.remote_control.port = self.config.remote_control_port;
        }
        gateway_config.computer.remote_control.protocol =
            self.config.remote_control_protocol.clone();
        if let Some(ref key) = self.config.remote_control_key {
            gateway_config.computer.remote_control.key_path = Some(key.clone());
        }
        if self.config.headless {
            gateway_config.computer.headless.enabled = true;
            gateway_config.computer.headless.display = self.config.headless_display.clone();
            println!("🖥️  Headless mode enabled on display {}", self.config.headless_display);
        }

        // Cloud opt-out (CLI override): cloud-compiled builds default the
        // runtime gate on; `--nocloud` forces it off regardless of config.toml.
        #[cfg(feature = "cloud")]
        if self.config.nocloud {
            gateway_config.cloud.enabled = false;
            println!("☁️  Cloud features disabled (--nocloud)");
        }
        // A binary built without the cloud feature has nothing to disable;
        // say so instead of silently ignoring the flag.
        #[cfg(not(feature = "cloud"))]
        if self.config.nocloud {
            println!("☁️  Cloud support not compiled in (--nocloud is a no-op)");
        }

        // Configure LLM Provider from environment variables (legacy support)
        apply_env_provider_overrides(&mut gateway_config);

        // Write PID file
        let pid = std::process::id();
        self.write_pid(pid).await?;

        // Create and start the Gateway
        let gateway = Gateway::new(gateway_config.clone(), Some(config_path.clone())).await?;

        // Startup recovery: scan for incomplete plans
        if let Err(e) = run_startup_recovery(&gateway).await {
            warn!("Startup recovery check failed: {}", e);
        }

        println!("✅ Gateway ready");
        println!("   URL: http://{}:{}", gateway_config.host, gateway_config.port);

        // Background self-update check (warms the web banner cache and logs).
        if gateway_config.update.enabled && gateway_config.update.auto_check {
            let shutdown = gateway.shutdown_token();
            tokio::spawn(async move {
                tokio::select! {
                    _ = crate::update::github::check_and_log(crate::VERSION) => {}
                    _ = shutdown.cancelled() => {}
                }
            });
        }

        // Watch for shutdown signals and cancel the gateway token so that
        // `gateway.start()` returns and we can run the full shutdown sequence.
        let shutdown_token = gateway.shutdown_token();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                #[allow(clippy::expect_used)] // signal handler install can't recover
                let mut sigterm =
                    signal(SignalKind::terminate()).expect("Failed to install SIGTERM handler");
                #[allow(clippy::expect_used)] // signal handler install can't recover
                let mut sigint =
                    signal(SignalKind::interrupt()).expect("Failed to install SIGINT handler");
                tokio::select! {
                    _ = sigterm.recv() => {},
                    _ = sigint.recv() => {},
                    _ = tokio::signal::ctrl_c() => {},
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
            shutdown_token.cancel();
        });

        let start_result = gateway.start().await;

        // Clean up PID file and run full gateway shutdown regardless of why
        // `start()` returned.
        let _ = tokio::fs::remove_file(&self.config.pid_file).await;
        if let Err(e) = gateway.stop().await {
            warn!("Gateway shutdown error: {}", e);
        }
        println!("\n👋 Daemon stopped");

        start_result
    }

    /// Stop the daemon gracefully
    pub async fn stop(&self) -> crate::Result<()> {
        match self.read_pid().await {
            Some(pid) => {
                if self.is_process_running(pid).await {
                    // Send SIGTERM
                    #[cfg(unix)]
                    {
                        use nix::sys::signal::{kill, Signal};
                        use nix::unistd::Pid;

                        kill(Pid::from_raw(pid as i32), Signal::SIGTERM).map_err(|e| {
                            crate::error::SyscityError::Internal(format!(
                                "Failed to send signal: {}",
                                e
                            ))
                        })?;
                    }

                    #[cfg(not(unix))]
                    {
                        // Windows: use taskkill
                        Command::new("taskkill")
                            .args(["/PID", &pid.to_string(), "/F"])
                            .output()
                            .await
                            .map_err(|e| crate::error::SyscityError::Io(e))?;
                    }

                    // Wait for process to exit
                    for _ in 0..50 {
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                        if !self.is_process_running(pid).await {
                            break;
                        }
                    }

                    // Remove PID file
                    let _ = tokio::fs::remove_file(&self.config.pid_file).await;
                    println!("✅ Syscity daemon stopped");
                } else {
                    println!("⚠️ Daemon was not running (removing stale PID file)");
                    let _ = tokio::fs::remove_file(&self.config.pid_file).await;
                }
                Ok(())
            }
            None => {
                println!("⚠️ Syscity daemon is not running");
                Ok(())
            }
        }
    }

    /// Force stop the daemon (SIGKILL)
    pub async fn stop_force(&self) -> crate::Result<()> {
        match self.read_pid().await {
            Some(pid) => {
                if self.is_process_running(pid).await {
                    // Send SIGKILL
                    #[cfg(unix)]
                    {
                        use nix::sys::signal::{kill, Signal};
                        use nix::unistd::Pid;

                        kill(Pid::from_raw(pid as i32), Signal::SIGKILL).map_err(|e| {
                            crate::error::SyscityError::Internal(format!(
                                "Failed to send signal: {}",
                                e
                            ))
                        })?;
                    }

                    #[cfg(not(unix))]
                    {
                        Command::new("taskkill")
                            .args(["/PID", &pid.to_string(), "/F"])
                            .output()
                            .await
                            .map_err(|e| crate::error::SyscityError::Io(e))?;
                    }

                    println!("✅ Syscity daemon force stopped");
                } else {
                    println!("⚠️ Daemon was not running");
                }

                // Remove PID file
                let _ = tokio::fs::remove_file(&self.config.pid_file).await;
                Ok(())
            }
            None => {
                println!("⚠️ Syscity daemon is not running");
                Ok(())
            }
        }
    }

    /// Check daemon status
    pub async fn status(&self) -> crate::Result<()> {
        match self.read_pid().await {
            Some(pid) => {
                if self.is_process_running(pid).await {
                    println!("✅ Syscity daemon is running");
                    println!("   PID: {}", pid);
                    println!("   Host: {}", self.config.host);
                    println!("   Port: {}", self.config.port);
                    println!("   URL: http://{}:{}", self.config.host, self.config.port);
                    println!("   PID file: {:?}", self.config.pid_file);
                } else {
                    println!("⚠️ Daemon is not running (stale PID file)");
                    let _ = tokio::fs::remove_file(&self.config.pid_file).await;
                }
                Ok(())
            }
            None => {
                println!("⚠️ Syscity daemon is not running");
                Ok(())
            }
        }
    }

    /// Read PID from file
    async fn read_pid(&self) -> Option<u32> {
        match tokio::fs::read_to_string(&self.config.pid_file).await {
            Ok(content) => content.trim().parse::<u32>().ok(),
            Err(_) => None,
        }
    }

    /// Write PID to file
    async fn write_pid(&self, pid: u32) -> crate::Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = self.config.pid_file.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(crate::error::SyscityError::Io)?;
        }

        tokio::fs::write(&self.config.pid_file, pid.to_string())
            .await
            .map_err(crate::error::SyscityError::Io)?;

        Ok(())
    }

    /// Check if a process is running
    pub async fn is_process_running(&self, pid: u32) -> bool {
        #[cfg(unix)]
        {
            use nix::unistd::Pid;

            // Send signal 0 to check if process exists
            nix::sys::signal::kill(Pid::from_raw(pid as i32), None).is_ok()
        }

        #[cfg(not(unix))]
        {
            // Windows: check via tasklist
            match Command::new("tasklist")
                .args(["/FI", &format!("PID eq {}", pid), "/NH"])
                .output()
                .await
            {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    stdout.contains(&pid.to_string())
                }
                Err(_) => false,
            }
        }
    }

    #[allow(dead_code)]
    /// Initialize the SQLite memory store
    async fn init_memory_store() -> crate::Result<crate::memory::SqliteMemoryStore> {
        // Use centralized ~/.syscity/memory directory
        let db_path = crate::dirs::default_memory_db();

        // Create the database file if it doesn't exist
        // SQLite requires the file to exist before connecting
        if !db_path.exists() {
            if let Some(parent) = db_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::File::create(&db_path).await?;
        }

        let db_url = format!("sqlite:///{}", db_path.display());

        println!("💾 Memory store: {}", db_path.display());

        crate::memory::SqliteMemoryStore::new(&db_url).await
    }
}

/// Scan for incomplete plans at startup and optionally resume them.
async fn run_startup_recovery(gateway: &crate::gateway::Gateway) -> crate::Result<()> {
    let db_path = crate::dirs::syscity_dir().join("planner.db");
    if !db_path.exists() {
        return Ok(());
    }

    let url = format!("sqlite:/// {}", db_path.display());
    let store = match crate::planner::TaskStateStore::new(&url).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Failed to open planner state store: {}", e);
            return Ok(());
        }
    };

    let summaries = match store.load_plan_summaries().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Failed to load plan summaries: {}", e);
            return Ok(());
        }
    };

    if summaries.is_empty() {
        return Ok(());
    }

    println!("\n📋 Interrupted plans detected:");
    println!("{:-<60}", "");
    for s in &summaries {
        println!(
            "  • {} (created: {})",
            s.goal,
            s.created_at.split('T').next().unwrap_or(&s.created_at)
        );
        println!(
            "    Progress: {}/{} completed, {} failed, {} pending",
            s.completed_tasks, s.total_tasks, s.failed_tasks, s.pending_tasks
        );
    }
    println!("{:-<60}\n", "");

    let auto_resume = std::env::var("SYSCITY_AUTO_RESUME")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);

    if auto_resume {
        tracing::info!("Auto-resume enabled — resuming interrupted plans");

        let registry = gateway.tool_registry();
        let adapter = crate::computer::create_adapter(registry).await?;
        let adapter: std::sync::Arc<dyn crate::computer::ComputerAdapter> =
            std::sync::Arc::from(adapter);

        let provider = gateway.model_router().create_default_provider().await?;

        let planner =
            crate::planner::GoalPlanner::with_provider(adapter, provider).with_state_store(store);

        for s in summaries {
            match planner.resume_plan(&s.id).await {
                Ok(Some(result)) => {
                    println!("✅ Plan '{}' resumed: {}", s.goal, result.message);
                }
                Ok(None) => {
                    tracing::warn!("Plan '{}' no longer exists", s.id);
                }
                Err(e) => {
                    tracing::warn!("Failed to resume plan '{}': {}", s.id, e);
                }
            }
        }
    } else {
        println!("Set SYSCITY_AUTO_RESUME=1 to automatically resume on startup.");
        println!("Or use the API / CLI to resume individual plans.\n");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    #[test]
    fn test_daemon_config_creation() {
        let config = DaemonConfig {
            host: "127.0.0.1".to_string(),
            port: 8080,
            pid_file: PathBuf::from("/tmp/syscity.pid"),
            remote_control_host: None,
            remote_control_user: None,
            remote_control_port: 22,
            remote_control_protocol: "ssh".to_string(),
            remote_control_key: None,
            headless: false,
            headless_display: ":99".to_string(),
            nocloud: false,
        };
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 8080);
        assert_eq!(config.pid_file, PathBuf::from("/tmp/syscity.pid"));
    }

    #[test]
    fn test_daemon_manager_new() {
        let config = DaemonConfig {
            host: "0.0.0.0".to_string(),
            port: 3000,
            pid_file: PathBuf::from("/tmp/syscity-test.pid"),
            remote_control_host: None,
            remote_control_user: None,
            remote_control_port: 22,
            remote_control_protocol: "ssh".to_string(),
            remote_control_key: None,
            headless: false,
            headless_display: ":99".to_string(),
            nocloud: false,
        };
        let manager = DaemonManager::new(config.clone());
        assert!(manager.is_ok());
        let manager = manager.unwrap();
        assert_eq!(manager.config.host, "0.0.0.0");
        assert_eq!(manager.config.port, 3000);
    }

    #[tokio::test]
    async fn test_write_and_read_pid() {
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("test.pid");

        let config = DaemonConfig {
            host: "127.0.0.1".to_string(),
            port: 8080,
            pid_file: pid_file.clone(),
            remote_control_host: None,
            remote_control_user: None,
            remote_control_port: 22,
            remote_control_protocol: "ssh".to_string(),
            remote_control_key: None,
            headless: false,
            headless_display: ":99".to_string(),
            nocloud: false,
        };
        let manager = DaemonManager::new(config).unwrap();

        // Write a PID
        manager.write_pid(12345).await.unwrap();
        assert!(pid_file.exists());

        // Read it back
        let pid = manager.read_pid().await;
        assert_eq!(pid, Some(12345));
    }

    #[tokio::test]
    async fn test_read_pid_missing_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("nonexistent.pid");

        let config = DaemonConfig {
            host: "127.0.0.1".to_string(),
            port: 8080,
            pid_file,
            remote_control_host: None,
            remote_control_user: None,
            remote_control_port: 22,
            remote_control_protocol: "ssh".to_string(),
            remote_control_key: None,
            headless: false,
            headless_display: ":99".to_string(),
            nocloud: false,
        };
        let manager = DaemonManager::new(config).unwrap();

        let pid = manager.read_pid().await;
        assert_eq!(pid, None);
    }

    #[tokio::test]
    async fn test_read_pid_invalid_content() {
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("invalid.pid");

        tokio::fs::write(&pid_file, "not-a-number").await.unwrap();

        let config = DaemonConfig {
            host: "127.0.0.1".to_string(),
            port: 8080,
            pid_file,
            remote_control_host: None,
            remote_control_user: None,
            remote_control_port: 22,
            remote_control_protocol: "ssh".to_string(),
            remote_control_key: None,
            headless: false,
            headless_display: ":99".to_string(),
            nocloud: false,
        };
        let manager = DaemonManager::new(config).unwrap();

        let pid = manager.read_pid().await;
        assert_eq!(pid, None);
    }

    #[tokio::test]
    async fn test_write_pid_creates_parent_dirs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("nested").join("dirs").join("test.pid");

        let config = DaemonConfig {
            host: "127.0.0.1".to_string(),
            port: 8080,
            pid_file: pid_file.clone(),
            remote_control_host: None,
            remote_control_user: None,
            remote_control_port: 22,
            remote_control_protocol: "ssh".to_string(),
            remote_control_key: None,
            headless: false,
            headless_display: ":99".to_string(),
            nocloud: false,
        };
        let manager = DaemonManager::new(config).unwrap();

        manager.write_pid(99999).await.unwrap();
        assert!(pid_file.exists());

        let content = tokio::fs::read_to_string(&pid_file).await.unwrap();
        assert_eq!(content, "99999");
    }

    #[tokio::test]
    async fn test_is_process_running_self() {
        let config = DaemonConfig {
            host: "127.0.0.1".to_string(),
            port: 8080,
            pid_file: PathBuf::from("/tmp/syscity.pid"),
            remote_control_host: None,
            remote_control_user: None,
            remote_control_port: 22,
            remote_control_protocol: "ssh".to_string(),
            remote_control_key: None,
            headless: false,
            headless_display: ":99".to_string(),
            nocloud: false,
        };
        let manager = DaemonManager::new(config).unwrap();

        let current_pid = std::process::id();
        assert!(manager.is_process_running(current_pid).await);
    }

    #[tokio::test]
    async fn test_is_process_running_fake_pid() {
        let config = DaemonConfig {
            host: "127.0.0.1".to_string(),
            port: 8080,
            pid_file: PathBuf::from("/tmp/syscity.pid"),
            remote_control_host: None,
            remote_control_user: None,
            remote_control_port: 22,
            remote_control_protocol: "ssh".to_string(),
            remote_control_key: None,
            headless: false,
            headless_display: ":99".to_string(),
            nocloud: false,
        };
        let manager = DaemonManager::new(config).unwrap();

        // PID 999999 is extremely unlikely to exist
        assert!(!manager.is_process_running(999999).await);
    }

    #[test]
    #[serial]
    fn test_env_first_overrides_shared_token() {
        use crate::gateway::{CredentialPrecedence, GatewayConfig};

        std::env::set_var("SYSCITY_SECURITY_SHARED_TOKEN", "env-token");
        let mut config = GatewayConfig::default();
        config.security.shared_token = Some("config-token".to_string());
        config.security.credential_precedence = CredentialPrecedence::EnvFirst;

        apply_env_security_overrides(&mut config);

        assert_eq!(config.security.shared_token.as_deref(), Some("env-token"));
        std::env::remove_var("SYSCITY_SECURITY_SHARED_TOKEN");
    }

    #[test]
    #[serial]
    fn test_config_first_keeps_shared_token() {
        use crate::gateway::{CredentialPrecedence, GatewayConfig};

        std::env::set_var("SYSCITY_SECURITY_SHARED_TOKEN", "env-token");
        let mut config = GatewayConfig::default();
        config.security.shared_token = Some("config-token".to_string());
        config.security.credential_precedence = CredentialPrecedence::ConfigFirst;

        apply_env_security_overrides(&mut config);

        assert_eq!(config.security.shared_token.as_deref(), Some("config-token"));
        std::env::remove_var("SYSCITY_SECURITY_SHARED_TOKEN");
    }

    #[test]
    #[serial]
    fn test_config_first_falls_back_to_env_shared_token() {
        use crate::gateway::{CredentialPrecedence, GatewayConfig};

        std::env::set_var("SYSCITY_SECURITY_SHARED_TOKEN", "env-token");
        let mut config = GatewayConfig::default();
        config.security.shared_token = None;
        config.security.credential_precedence = CredentialPrecedence::ConfigFirst;

        apply_env_security_overrides(&mut config);

        assert_eq!(config.security.shared_token.as_deref(), Some("env-token"));
        std::env::remove_var("SYSCITY_SECURITY_SHARED_TOKEN");
    }

    #[test]
    #[serial]
    fn test_env_first_overrides_provider() {
        use crate::gateway::{CredentialPrecedence, GatewayConfig};
        use crate::model_router::{ProviderConfig, ProviderType};

        std::env::set_var("OPENAI_API_KEY", "env-openai-key");
        let mut config = GatewayConfig::default();
        config.providers.insert(
            "openai".to_string(),
            ProviderConfig {
                provider_type: ProviderType::OpenAi,
                models: vec!["gpt-4o".to_string()],
                default_model: "gpt-4o".to_string(),
                api_key: "config-openai-key".to_string().into(),
                api_keys: Vec::new(),
                auth_profile: None,
                oauth: None,
                base_url: None,
                timeout: std::time::Duration::from_secs(60),
                max_retries: 3,
                retry_delay_ms: 1000,
            },
        );
        config.security.credential_precedence = CredentialPrecedence::EnvFirst;

        apply_env_provider_overrides(&mut config);

        assert_eq!(
            config.providers["openai"].api_key,
            crate::model_router::ProviderKey::Inline("env-openai-key".to_string())
        );
        std::env::remove_var("OPENAI_API_KEY");
    }

    #[test]
    #[serial]
    fn test_config_first_keeps_provider() {
        use crate::gateway::{CredentialPrecedence, GatewayConfig};
        use crate::model_router::{ProviderConfig, ProviderType};

        std::env::set_var("OPENAI_API_KEY", "env-openai-key");
        let mut config = GatewayConfig::default();
        config.providers.insert(
            "openai".to_string(),
            ProviderConfig {
                provider_type: ProviderType::OpenAi,
                models: vec!["gpt-4o".to_string()],
                default_model: "gpt-4o".to_string(),
                api_key: "config-openai-key".to_string().into(),
                api_keys: Vec::new(),
                auth_profile: None,
                oauth: None,
                base_url: None,
                timeout: std::time::Duration::from_secs(60),
                max_retries: 3,
                retry_delay_ms: 1000,
            },
        );
        config.security.credential_precedence = CredentialPrecedence::ConfigFirst;

        apply_env_provider_overrides(&mut config);

        assert_eq!(
            config.providers["openai"].api_key,
            crate::model_router::ProviderKey::Inline("config-openai-key".to_string())
        );
        std::env::remove_var("OPENAI_API_KEY");
    }

    #[test]
    #[serial]
    fn test_config_first_adds_missing_provider_from_env() {
        use crate::gateway::{CredentialPrecedence, GatewayConfig};

        std::env::set_var("ANTHROPIC_API_KEY", "env-anthropic-key");
        let mut config = GatewayConfig::default();
        config.security.credential_precedence = CredentialPrecedence::ConfigFirst;

        apply_env_provider_overrides(&mut config);

        assert_eq!(
            config.providers["anthropic"].api_key,
            crate::model_router::ProviderKey::Inline("env-anthropic-key".to_string())
        );
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
}
