//! Command-line interface for Syscity
//!
//! This module handles argument parsing and command execution
//! using the `clap` crate.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::config::Config;
use crate::error::Result;

// Subcommand modules
mod admin;
mod agent;
mod approval;
mod audit;
mod auth;
mod capability;
mod channel;
mod config_cmd;
mod cost;
mod cron;
mod daemon;
mod device;
mod doctor;
mod eval;
mod export;
mod goal;
mod kb;
mod mcp;
mod memory;
mod observe;
mod plugin;
mod provider;
mod secrets;
mod security;
mod session;
mod setup;
mod skill;
mod update;
mod ws;

pub use admin::AdminCommands;
pub use agent::AgentCommands;
pub use approval::ApprovalCommands;
pub use audit::AuditCommands;
pub use auth::AuthCommands;
pub use channel::ChannelCommands;
pub use config_cmd::ConfigCommands;
pub use cost::CostCommands;
pub use cron::CronCommands;
pub use device::DeviceCommands;
pub use doctor::DoctorCommands;
pub use doctor::{DiagnosticHint, HintSeverity};
pub use eval::EvalCommands;
pub use export::ExportCommands;
pub use goal::GoalCommands;
pub use kb::KbCommands;
pub use mcp::McpCommands;
pub use memory::MemoryCommands;
pub use observe::ObserveCommands;
pub use plugin::PluginCommands;
pub use provider::ProviderCommands;
pub use secrets::SecretsCommands;
pub use security::{GateCommands, PairingCommands, SecurityCommands};
pub use session::SessionCommands;
pub use skill::SkillCommands;
pub use update::UpdateCommands;

/// Syscity - Your AI assistant
#[derive(Debug, Parser)]
#[command(name = "syscity")]
#[command(about = "Syscity - Your AI assistant")]
#[command(version)]
pub struct Cli {
    /// Configuration file path
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Log level override (trace, debug, info, warn, error)
    #[arg(short, long, global = true)]
    pub log_level: Option<String>,

    /// Subcommand to execute
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Evaluation commands (list suites, validate YAML, run evals)
    Eval {
        /// Eval subcommand
        #[command(subcommand)]
        command: EvalCommands,
    },
    /// Export conversations and memories to files
    Export {
        /// Export subcommand
        #[command(subcommand)]
        command: ExportCommands,
    },
    /// Goal management (list, resume, cancel)
    Goal {
        /// Goal subcommand
        #[command(subcommand)]
        command: GoalCommands,
    },
    /// Configuration management (get, set, validate)
    Config {
        /// Config subcommand
        #[command(subcommand)]
        command: Option<ConfigCommands>,
    },
    /// Health check
    Health,
    /// Run the registered runtime invariant checks (module-owned data
    /// guarantees; see src/core/invariants.rs)
    Invariants {
        /// Emit machine-readable JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Run as an assistant process (internal use)
    AssistantRun {
        /// Configuration file path
        #[arg(short, long)]
        config: PathBuf,
    },
    /// Admin commands for Gateway management (provider switching, status, etc.)
    Admin {
        /// Admin subcommand
        #[command(subcommand)]
        command: AdminCommands,
    },
    /// Cron job management
    Cron {
        /// Cron subcommand
        #[command(subcommand)]
        command: CronCommands,
    },
    /// Skill management commands
    Skill {
        /// Skill subcommand
        #[command(subcommand)]
        command: SkillCommands,
    },
    /// Agent personality management (memory files)
    Agent {
        /// Agent subcommand
        #[command(subcommand)]
        command: AgentCommands,
    },
    /// Channel management (Telegram, Discord, Slack)
    Channel {
        /// Channel subcommand
        #[command(subcommand)]
        command: ChannelCommands,
    },
    /// Plugin management for WASM channel extensions
    #[command(name = "plugin")]
    Plugin {
        /// Plugin subcommand
        #[command(subcommand)]
        command: PluginCommands,
    },
    /// Start the Syscity daemon (background server)
    Start {
        /// Host to bind to
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port for gateway API, WebSocket, and SPA
        #[arg(short, long, default_value = "18080")]
        port: u16,
        /// Run in foreground (don't detach)
        #[arg(long)]
        foreground: bool,
        /// Remote control target host
        #[arg(long)]
        remote_control_host: Option<String>,
        /// Remote control username
        #[arg(long)]
        remote_control_user: Option<String>,
        /// Remote control port
        #[arg(long, default_value = "22")]
        remote_control_port: u16,
        /// Remote control protocol (ssh, vnc, rdp)
        #[arg(long, default_value = "ssh")]
        remote_control_protocol: String,
        /// Path to SSH private key for remote control
        #[arg(long)]
        remote_control_key: Option<String>,
        /// Enable headless display mode
        #[arg(long)]
        headless: bool,
        /// Headless display identifier (e.g. ":99")
        #[arg(long, default_value = ":99")]
        headless_display: String,
        /// Force-disable cloud features (cloud is on by default)
        #[arg(long)]
        nocloud: bool,
    },
    /// Stop the Syscity daemon
    Stop {
        /// Force kill if graceful shutdown fails
        #[arg(short, long)]
        force: bool,
    },
    /// Reload plugins and configuration without restarting daemon
    Reload,
    /// Check daemon status
    Status,
    /// Restart the daemon (used internally by the self-update helper)
    Restart {
        /// PID of the old daemon process to wait for before starting
        #[arg(long)]
        pid: Option<u32>,
        /// Host to bind to
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port for gateway API, WebSocket, and SPA
        #[arg(short, long, default_value = "18080")]
        port: u16,
        /// Force-disable cloud features (preserves a `start --nocloud` across
        /// self-update restarts)
        #[arg(long)]
        nocloud: bool,
    },
    /// Show and tail daemon logs
    Logs {
        /// Number of lines to show (default: 50)
        #[arg(short = 'n', long, default_value = "50")]
        lines: usize,
        /// Follow/tail the logs (like tail -f)
        #[arg(short, long)]
        follow: bool,
    },
    /// MCP (Model Context Protocol) management
    Mcp {
        /// MCP subcommand
        #[command(subcommand)]
        command: McpCommands,
    },
    /// Vector memory management (search, add)
    Memory {
        /// Memory subcommand
        #[command(subcommand)]
        command: MemoryCommands,
    },
    /// Knowledge Base management (ingest, list, delete)
    Kb {
        /// KB subcommand
        #[command(subcommand)]
        command: KbCommands,
    },
    /// Security audit, DM pairing, and access control
    Security {
        /// Security subcommand
        #[command(subcommand)]
        command: SecurityCommands,
    },
    /// Session, thread, and turn management (introspect & undo)
    Session {
        /// Session subcommand
        #[command(subcommand)]
        command: SessionCommands,
    },
    /// Per-turn observability records (stats, list, show, export, prune)
    Observe {
        /// Observe subcommand
        #[command(subcommand)]
        command: ObserveCommands,
    },
    /// Self-update from GitHub Releases (bare `update` installs the latest)
    Update {
        /// Update subcommand (bare `syscity update` installs the latest)
        #[command(subcommand)]
        command: Option<UpdateCommands>,
    },
    /// Secret store management (list, migrate, purge)
    Secrets {
        /// Secrets subcommand
        #[command(subcommand)]
        command: SecretsCommands,
    },
    /// Initialize Syscity with an interactive setup wizard
    Setup,
    /// Device pairing management
    Device {
        /// Device subcommand
        #[command(subcommand)]
        command: DeviceCommands,
    },
    /// Tool approval management (human-in-the-loop)
    Approval {
        /// Approval subcommand
        #[command(subcommand)]
        command: ApprovalCommands,
    },
    Cost {
        /// Cost-guard subcommand
        #[command(subcommand)]
        command: CostCommands,
    },
    /// Audit log and security audit
    Audit {
        /// Audit subcommand
        #[command(subcommand)]
        command: AuditCommands,
    },
    /// Auth management (shared token lifecycle, mode)
    Auth {
        /// Auth subcommand
        #[command(subcommand)]
        command: AuthCommands,
    },
    /// Provider management (list, enable, disable, switch)
    Provider {
        /// Provider subcommand
        #[command(subcommand)]
        command: ProviderCommands,
    },
    /// Diagnostic system (run checks, view reports)
    Doctor {
        /// Doctor subcommand
        #[command(subcommand)]
        command: DoctorCommands,
    },
    /// Check and display available OS capability sets
    Capabilities,
    /// Interactive terminal UI client
    Tui {
        /// Host to connect to
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to connect to
        #[arg(short, long, default_value = "18080")]
        port: u16,
        /// Auth token (for token auth mode)
        #[arg(long)]
        token: Option<String>,
        /// Resume the most recently active session
        #[arg(short = 'c', long = "continue", conflicts_with_all = ["resume", "session"])]
        r#continue: bool,
        /// Resume a specific session (id, or a number from the /resume list)
        #[arg(long, num_args = 0..=1, default_missing_value = "", conflicts_with = "session")]
        resume: Option<String>,
        /// Pre-select a session ID (alias for --resume <id>)
        #[arg(long)]
        session: Option<String>,
    },
}

// AgentCommands is defined in agent.rs and re-exported here
// PluginCommands is defined in plugin.rs and re-exported here

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ChannelType {
    /// Telegram bot
    Telegram,
    /// Discord bot
    Discord,
    /// Slack bot
    Slack,
    /// WhatsApp bot
    Whatsapp,
    /// QQ bot
    Qq,
    /// Feishu/Lark bot
    Feishu,
    /// Custom WebSocket endpoint
    Websocket,
    /// Signal via signal-cli
    Signal,
    /// iMessage via BlueBubbles
    Imessage,
    /// WebChat browser interface
    Webchat,
    /// WeChat Official Account (公众号) webhook
    WechatMp,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    Json,
    Yaml,
    Table,
    Plain,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ConfigFormat {
    Toml,
    Json,
    Yaml,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum StorageType {
    /// SQLite storage (default, embedded)
    Sqlite,
    /// PostgreSQL storage (requires external database)
    Postgres,
    /// Redis storage (for caching and pub/sub)
    Redis,
}

fn init_logging(log_level: Option<&str>) {
    let level = log_level.unwrap_or("info");
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(format!("syscity={},hyper=warn,reqwest=warn", level))
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .compact()
        .finish();

    if tracing::subscriber::set_global_default(subscriber).is_err() {
        eprintln!("warning: failed to set global tracing subscriber");
    }
}

impl Cli {
    /// Run the CLI
    pub async fn run() -> Result<()> {
        let cli = Cli::parse();

        // Initialize logging
        init_logging(cli.log_level.as_deref());

        // Load configuration
        let config = if let Some(config_path) = &cli.config {
            Config::load_with_file(Some(config_path))?
        } else {
            Config::load()?
        };

        // Execute command
        cli.execute(&config).await
    }

    /// Execute the CLI command
    pub async fn execute(&self, config: &Config) -> Result<()> {
        match &self.command {
            Commands::Eval { command } => command.run(config).await,
            Commands::Export { command } => export::run_export_command(command).await,
            Commands::Goal { command } => goal::run_goal_command(command).await,
            Commands::Config { command } => match command {
                Some(cmd) => config_cmd::run_config_command(cmd).await,
                None => setup::run_setup().await,
            },
            Commands::Health => daemon::run_health_check(config).await,
            Commands::Invariants { json } => run_invariants(*json).await,
            Commands::AssistantRun { config: config_path } => {
                daemon::run_assistant_process(config_path).await
            }
            Commands::Admin { command } => admin::run_admin_command(command).await,
            Commands::Cron { command } => cron::run_cron_command(command).await,
            Commands::Skill { command } => skill::run_skill_command(command).await,
            Commands::Agent { command } => agent::run_agent_command(command).await,
            Commands::Channel { command } => channel::run_channel_command(command).await,
            Commands::Plugin { command } => plugin::run_plugin_command(command).await,
            Commands::Start {
                host,
                port,
                foreground,
                remote_control_host,
                remote_control_user,
                remote_control_port,
                remote_control_protocol,
                remote_control_key,
                headless,
                headless_display,
                nocloud,
            } => {
                let daemon_config = crate::daemon::DaemonConfig {
                    host: host.clone(),
                    port: *port,
                    pid_file: std::path::PathBuf::new(),
                    remote_control_host: remote_control_host.clone(),
                    remote_control_user: remote_control_user.clone(),
                    remote_control_port: *remote_control_port,
                    remote_control_protocol: remote_control_protocol.clone(),
                    remote_control_key: remote_control_key.clone(),
                    headless: *headless,
                    headless_display: headless_display.clone(),
                    nocloud: *nocloud,
                };
                daemon::run_start_daemon(*foreground, config, daemon_config).await
            }
            Commands::Stop { force } => daemon::run_stop_daemon(*force).await,
            Commands::Reload => daemon::run_reload_daemon().await,
            Commands::Status => daemon::run_daemon_status().await,
            Commands::Restart { pid, host, port, nocloud } => {
                daemon::run_restart_daemon(*pid, host, *port, *nocloud).await
            }
            Commands::Logs { lines, follow } => daemon::run_logs(*lines, *follow).await,
            Commands::Mcp { command } => mcp::run_mcp_command(command).await,
            Commands::Memory { command } => memory::run_memory_command(command).await,
            Commands::Kb { command } => kb::run_kb_command(command).await,
            Commands::Security { command } => security::run_security_command(command).await,
            Commands::Session { command } => session::run_session_command(command).await,
            Commands::Observe { command } => observe::run_observe_command(command).await,
            Commands::Update { command } => update::run_update_command(command).await,
            Commands::Secrets { command } => secrets::run_secrets_command(command).await,
            Commands::Setup => setup::run_setup().await,
            Commands::Device { command } => device::run_device_command(command).await,
            Commands::Approval { command } => approval::run_approval_command(command).await,
            Commands::Cost { command } => cost::run_cost_command(command).await,
            Commands::Audit { command } => audit::run_audit_command(command).await,
            Commands::Auth { command } => auth::run_auth_command(command).await,
            Commands::Provider { command } => provider::run_provider_command(command, config).await,
            Commands::Doctor { command } => doctor::run_doctor_command(command).await,
            Commands::Capabilities => capability::run_capability_check().await,
            Commands::Tui {
                host,
                port,
                token,
                r#continue,
                resume,
                session,
            } => {
                // `--resume` with no value lists the sessions and lets the user
                // pick; `--session` remains as an alias for `--resume <id>`.
                let choice = if *r#continue {
                    crate::tui::SessionChoice::Continue
                } else if let Some(id) = resume.as_deref().filter(|s| !s.is_empty()) {
                    crate::tui::SessionChoice::Resume(id.to_string())
                } else if resume.is_some() {
                    crate::tui::SessionChoice::Resume(String::new())
                } else if let Some(id) = session {
                    crate::tui::SessionChoice::Resume(id.clone())
                } else {
                    crate::tui::SessionChoice::New
                };
                crate::tui::run(host, *port, token.as_deref(), choice).await
            }
        }
    }
}

/// Run every registered runtime invariant check and print the report.
///
/// Returns an error when any invariant is violated so the process exits
/// non-zero (CI / cron-friendly). Checks that report "not applicable"
/// (e.g. no store on disk yet) count as passed with a note.
async fn run_invariants(json: bool) -> Result<()> {
    use crate::core::invariants::{register_builtins, run_all};

    register_builtins();
    let report = run_all().await;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(crate::error::SyscityError::Serialization)?
        );
    } else {
        println!("Runtime invariants ({} registered):", report.len());
        for outcome in &report {
            let mark = if outcome.passed { "✓" } else { "✗" };
            println!("  {} [{}] {}", mark, outcome.module, outcome.id);
            if let Some(detail) = &outcome.detail {
                println!("      {}", detail);
            }
        }
    }

    let failures: Vec<&crate::core::invariants::InvariantOutcome> =
        report.iter().filter(|o| !o.passed).collect();
    if failures.is_empty() {
        println!("All invariants hold.");
        return Ok(());
    }
    Err(crate::error::SyscityError::Internal(format!(
        "{} of {} invariant(s) violated",
        failures.len(),
        report.len()
    )))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn parse_health_command() {
        let cli = Cli::try_parse_from(["syscity", "health"]).unwrap();
        assert!(matches!(cli.command, Commands::Health));
    }

    #[test]
    fn parse_config_show_command() {
        let cli = Cli::try_parse_from(["syscity", "config", "show"]).unwrap();
        assert!(matches!(cli.command, Commands::Config { .. }));
    }

    #[test]
    fn parse_config_get_command() {
        let cli = Cli::try_parse_from(["syscity", "config", "get", "gateway.host"]).unwrap();
        assert!(matches!(cli.command, Commands::Config { .. }));
    }

    #[test]
    fn parse_config_set_command() {
        let cli =
            Cli::try_parse_from(["syscity", "config", "set", "gateway.host=0.0.0.0"]).unwrap();
        assert!(matches!(cli.command, Commands::Config { .. }));
    }

    #[test]
    fn parse_config_without_subcommand() {
        let cli = Cli::try_parse_from(["syscity", "config"]).unwrap();
        match cli.command {
            Commands::Config { command } => assert!(command.is_none()),
            _ => panic!("expected Config command"),
        }
    }

    // NOTE: `start` command has a clap conflict: `-h` is used by both `host`
    // and `--help`. This is a bug in the CLI definition.
    // #[test]
    // fn parse_start_command_with_custom_port() { ... }

    #[test]
    fn parse_reload_command() {
        let cli = Cli::try_parse_from(["syscity", "reload"]).unwrap();
        assert!(matches!(cli.command, Commands::Reload));
    }

    #[test]
    fn parse_status_command() {
        let cli = Cli::try_parse_from(["syscity", "status"]).unwrap();
        assert!(matches!(cli.command, Commands::Status));
    }

    #[test]
    fn parse_stop_command_with_force() {
        let cli = Cli::try_parse_from(["syscity", "stop", "--force"]).unwrap();
        match cli.command {
            Commands::Stop { force } => assert!(force),
            _ => panic!("expected Stop command"),
        }
    }

    #[test]
    fn parse_logs_command_defaults() {
        let cli = Cli::try_parse_from(["syscity", "logs"]).unwrap();
        match cli.command {
            Commands::Logs { lines, follow } => {
                assert_eq!(lines, 50);
                assert!(!follow);
            }
            _ => panic!("expected Logs command"),
        }
    }

    #[test]
    fn parse_logs_command_with_follow() {
        let cli = Cli::try_parse_from(["syscity", "logs", "--follow", "-n", "100"]).unwrap();
        match cli.command {
            Commands::Logs { lines, follow } => {
                assert_eq!(lines, 100);
                assert!(follow);
            }
            _ => panic!("expected Logs command"),
        }
    }

    #[test]
    fn parse_admin_status_subcommand() {
        let cli = Cli::try_parse_from(["syscity", "admin", "status"]).unwrap();
        match cli.command {
            Commands::Admin { command } => {
                assert!(matches!(command, AdminCommands::Status));
            }
            _ => panic!("expected Admin command"),
        }
    }

    // NOTE: `plugin list` has a clap conflict: `-l` is used by both `loaded`
    // and the global `--log-level`. This is a bug in the CLI definition.
    // #[test]
    // fn parse_plugin_list_subcommand() { ... }

    #[test]
    fn parse_secrets_list_subcommand() {
        let cli = Cli::try_parse_from(["syscity", "secrets", "list"]).unwrap();
        match cli.command {
            Commands::Secrets { command } => assert!(matches!(command, SecretsCommands::List)),
            _ => panic!("expected Secrets command"),
        }
    }

    #[test]
    fn parse_secrets_migrate_subcommand() {
        let cli = Cli::try_parse_from(["syscity", "secrets", "migrate"]).unwrap();
        match cli.command {
            Commands::Secrets { command } => assert!(matches!(command, SecretsCommands::Migrate)),
            _ => panic!("expected Secrets command"),
        }
    }

    #[test]
    fn parse_secrets_purge_subcommand() {
        let cli = Cli::try_parse_from(["syscity", "secrets", "purge", "channel"]).unwrap();
        match cli.command {
            Commands::Secrets { command } => {
                assert!(matches!(command, SecretsCommands::Purge { .. }));
            }
            _ => panic!("expected Secrets command"),
        }
    }

    #[test]
    fn parse_security_pairing_list() {
        let cli = Cli::try_parse_from(["syscity", "security", "pairing", "list"]).unwrap();
        match cli.command {
            Commands::Security { command } => {
                assert!(matches!(
                    command,
                    SecurityCommands::Pairing {
                        command: PairingCommands::List { channel: None }
                    }
                ));
            }
            _ => panic!("expected Security command"),
        }
    }

    #[test]
    fn parse_with_log_level() {
        let cli = Cli::try_parse_from(["syscity", "--log-level", "debug", "status"]).unwrap();
        assert_eq!(cli.log_level, Some("debug".to_string()));
    }

    #[test]
    fn parse_with_config_path() {
        let cli = Cli::try_parse_from(["syscity", "-c", "/tmp/config.toml", "health"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/config.toml")));
    }

    #[test]
    fn parse_doctor_run_command() {
        let cli = Cli::try_parse_from(["syscity", "doctor", "run"]).unwrap();
        match cli.command {
            Commands::Doctor { command } => {
                assert!(matches!(command, DoctorCommands::Run { provider: None, verbose: false }));
            }
            _ => panic!("expected Doctor command"),
        }
    }

    #[test]
    fn parse_doctor_run_with_provider_and_verbose() {
        let cli = Cli::try_parse_from([
            "syscity",
            "doctor",
            "run",
            "--provider",
            "openai",
            "--verbose",
        ])
        .unwrap();
        match cli.command {
            Commands::Doctor { command } => {
                assert!(matches!(
                    command,
                    DoctorCommands::Run {
                        provider: Some(ref p),
                        verbose: true
                    } if p == "openai"
                ));
            }
            _ => panic!("expected Doctor command"),
        }
    }

    #[test]
    fn parse_doctor_report_command() {
        let cli = Cli::try_parse_from(["syscity", "doctor", "report"]).unwrap();
        match cli.command {
            Commands::Doctor { command } => {
                assert!(matches!(command, DoctorCommands::Report));
            }
            _ => panic!("expected Doctor command"),
        }
    }

    #[test]
    fn parse_unknown_command_fails() {
        let result = Cli::try_parse_from(["syscity", "unknown-cmd"]);
        assert!(result.is_err());
    }
}
