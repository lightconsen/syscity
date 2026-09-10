//! Tool subsystem initialization.
//!
//! Creates the tool registry, MCP manager, approval queue, canvas manager,
//! plugin manager, channel registry, and computer adapter.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn};

use crate::acp::AcpControlPlane;
use crate::agent::session_store::SessionStore;
use crate::canvas::CanvasManager;
use crate::channels::{Channel, ChannelExtensionRegistry, IncomingMessage};
use crate::computer::ComputerAdapter;
use crate::gateway::GatewayConfig;
use crate::hooks::ShellHookBridge;
use crate::mcp::{McpEvent, McpManager};
use crate::memory::MemoryManager;
use crate::model_router::ModelRouter;
use crate::plugins::PluginManager;
use crate::security::content_filter::ContentFilter;
use crate::security::runtime_audit::AuditLogger;
use crate::tools::approval::ApprovalQueue;
use crate::tools::ask_user::AskQueue;
use crate::tools::ToolRegistry;

/// Tool subsystem initialization result.
pub struct ToolsInit {
    pub mcp_manager: Arc<McpManager>,
    pub connector_manager: Arc<crate::mcp::ConnectorManager>,
    pub mcp_event_rx: mpsc::UnboundedReceiver<McpEvent>,
    pub approval_queue: Arc<ApprovalQueue>,
    pub ask_queue: Arc<AskQueue>,
    pub memory_manager_holder: Arc<RwLock<Option<Arc<MemoryManager>>>>,
    pub tool_registry: Arc<ToolRegistry>,
    pub computer_adapter: Option<Arc<dyn ComputerAdapter>>,
    pub planner_handle: Arc<std::sync::RwLock<Option<Arc<crate::planner::GoalPlanner>>>>,
    pub plugin_manager: Arc<PluginManager>,
    pub canvas_manager: Arc<CanvasManager>,
    pub channels: Arc<RwLock<HashMap<String, Arc<dyn Channel>>>>,
    pub channel_extensions: Arc<RwLock<ChannelExtensionRegistry>>,
}

/// Initialize MCP manager with its internal event channel.
pub async fn init_mcp_manager(
    secrets: Arc<crate::secrets::SecretStoreHandle>,
) -> (Arc<McpManager>, mpsc::UnboundedReceiver<McpEvent>) {
    let (mcp_event_tx, mcp_event_rx) = mpsc::unbounded_channel::<McpEvent>();
    let mcp_manager = Arc::new(McpManager::new(secrets).with_event_tx(mcp_event_tx).await);
    (mcp_manager, mcp_event_rx)
}

/// Initialize the computer / desktop automation adapter.
pub async fn init_computer_adapter(
    config: &GatewayConfig,
    tool_registry: Arc<ToolRegistry>,
) -> Option<Arc<dyn ComputerAdapter>> {
    if !config.computer.enabled {
        return None;
    }

    if let Some(ref host) = config.computer.remote_control.host {
        let rc_config = crate::computer::RemoteControlConfig {
            host: host.clone(),
            user: config
                .computer
                .remote_control
                .user
                .clone()
                .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "user".to_string())),
            port: config.computer.remote_control.port,
            protocol: crate::computer::RemoteProtocol::Ssh {
                key_path: config.computer.remote_control.key_path.clone(),
            },
            display: config.computer.remote_control.display.clone(),
            ssh_extra_args: config.computer.remote_control.ssh_extra_args.clone(),
            connect_timeout: std::time::Duration::from_secs(
                config.computer.remote_control.timeout_secs,
            ),
        };
        match crate::computer::RemoteControlAdapter::new(rc_config).await {
            Ok(adapter) => {
                info!("Remote control adapter connected to {} for desktop automation", host);
                return Some(Arc::new(adapter));
            }
            Err(e) => {
                warn!(
                    "Failed to connect remote control adapter to {}: {}. Falling back to local \
                     adapter.",
                    host, e
                );
            }
        }
    }

    match crate::computer::create_adapter(tool_registry.clone()).await {
        Ok(adapter) => {
            info!("Computer adapter initialized for desktop automation");
            Some(Arc::from(adapter))
        }
        Err(e) => {
            warn!("Failed to initialize computer adapter: {}", e);
            None
        }
    }
}

/// Initialize the plugin manager and wire provider callbacks.
pub async fn init_plugin_manager(
    _config: &GatewayConfig,
    tool_registry: Arc<ToolRegistry>,
    model_router: Arc<ModelRouter>,
    channels: Arc<RwLock<HashMap<String, Arc<dyn Channel>>>>,
    task_registry: Arc<crate::gateway::task_registry::TaskRegistry>,
) -> crate::Result<Arc<PluginManager>> {
    let plugins_dir = crate::dirs::config_dir().join("plugins");
    let plugin_manager = {
        let pm = PluginManager::new(plugins_dir).await?;
        pm.set_tool_registry(tool_registry);
        Arc::new(pm)
    };

    // Background task that registers plugin callback spawns in the unified
    // TaskRegistry. Callbacks are synchronous, so they send handles over this
    // channel and the async loop registers them with unique names.
    let (spawn_tx, mut spawn_rx) =
        mpsc::unbounded_channel::<(String, tokio::task::JoinHandle<()>)>();
    let registry_task_registry = task_registry.clone();
    let registry_task = tokio::spawn(async move {
        while let Some((name, handle)) = spawn_rx.recv().await {
            registry_task_registry.insert_join(name, handle).await;
        }
    });
    task_registry
        .insert_abort("plugin:spawn_registry", &registry_task)
        .await;

    // Wire plugin manager to register plugin-backed providers with the model router
    {
        let mr_register = model_router.clone();
        let mr_unregister = model_router.clone();
        let tx_register = spawn_tx.clone();
        let tx_unregister = spawn_tx.clone();
        plugin_manager.set_provider_callbacks(
            Arc::new(
                move |name: String, provider: Arc<dyn crate::providers::Provider + Send + Sync>| {
                    let task_name = name.clone();
                    let mr = mr_register.clone();
                    let tx = tx_register.clone();
                    let handle = tokio::spawn(async move {
                        if let Err(e) = mr.add_provider_instance(&task_name, provider).await {
                            warn!("Failed to register plugin provider '{}': {}", task_name, e);
                        }
                    });
                    if let Err(e) = tx.send((format!("plugin:provider:register:{}", name), handle))
                    {
                        warn!(
                            "Failed to enqueue plugin provider registration task '{}': {}",
                            name, e
                        );
                    }
                },
            ),
            Arc::new(move |name: String| {
                let task_name = name.clone();
                let mr = mr_unregister.clone();
                let tx = tx_unregister.clone();
                let handle = tokio::spawn(async move {
                    if let Err(e) = mr.remove_provider(&task_name).await {
                        warn!("Failed to unregister plugin provider '{}': {}", task_name, e);
                    }
                });
                if let Err(e) = tx.send((format!("plugin:provider:unregister:{}", name), handle)) {
                    warn!(
                        "Failed to enqueue plugin provider unregistration task '{}': {}",
                        name, e
                    );
                }
            }),
        );
    }

    // Wire plugin manager channel callbacks
    #[cfg(feature = "plugins")]
    {
        let (plugin_inbound_tx, _plugin_inbound_rx) = mpsc::unbounded_channel::<IncomingMessage>();

        let channels_reg = channels.clone();
        let channels_unreg = channels.clone();
        let tx_reg = spawn_tx.clone();
        let tx_unreg = spawn_tx.clone();
        plugin_manager.set_channel_callbacks(
            Arc::new(
                move |name: String, channel: Arc<dyn crate::channels::Channel + Send + Sync>| {
                    let task_name = name.clone();
                    let ch = channels_reg.clone();
                    let tx = tx_reg.clone();
                    let handle = tokio::spawn(async move {
                        ch.write().await.insert(task_name.clone(), channel);
                        info!("Registered plugin channel '{}'", task_name);
                    });
                    if let Err(e) = tx.send((format!("plugin:channel:register:{}", name), handle)) {
                        warn!(
                            "Failed to enqueue plugin channel registration task '{}': {}",
                            name, e
                        );
                    }
                },
            ),
            Arc::new(move |name: String| {
                let task_name = name.clone();
                let ch = channels_unreg.clone();
                let tx = tx_unreg.clone();
                let handle = tokio::spawn(async move {
                    ch.write().await.remove(&task_name);
                    info!("Deregistered plugin channel '{}'", task_name);
                });
                if let Err(e) = tx.send((format!("plugin:channel:unregister:{}", name), handle)) {
                    warn!("Failed to enqueue plugin channel unregistration task '{}': {}", name, e);
                }
            }),
        );

        plugin_manager.set_channel_message_tx(plugin_inbound_tx);
    }

    Ok(plugin_manager)
}

/// Shared handles the tool subsystem needs at boot (each a distinct
/// process-wide service wired together during gateway startup).
pub struct ToolSystemDeps {
    pub acp: Arc<AcpControlPlane>,
    pub session_store: Option<Arc<SessionStore>>,
    pub audit_log_dyn: Arc<dyn AuditLogger>,
    pub model_router: Arc<ModelRouter>,
    pub task_registry: Arc<crate::gateway::task_registry::TaskRegistry>,
    pub device_bridge: Option<Arc<dyn crate::device::DeviceBridge>>,
    pub skills_manager: Arc<RwLock<crate::skills::SkillManager>>,
    pub shell_hooks: Arc<ShellHookBridge>,
    /// Secret-store instance handle shared with the gateway.
    pub secrets: Arc<crate::secrets::SecretStoreHandle>,
}

/// Initialize the full tool subsystem.
pub async fn init_tools(config: &GatewayConfig, deps: ToolSystemDeps) -> crate::Result<ToolsInit> {
    let ToolSystemDeps {
        acp,
        session_store,
        audit_log_dyn,
        model_router,
        task_registry,
        device_bridge,
        skills_manager,
        shell_hooks,
        secrets,
    } = deps;
    let (mcp_manager, mcp_event_rx) = init_mcp_manager(secrets.clone()).await;
    let connector_manager = Arc::new(crate::mcp::ConnectorManager::new(
        crate::dirs::connectors_dir(),
        mcp_manager.clone(),
        Arc::new(crate::skills::SkillStorage::new()?),
        // kind=cloud connectors route through the cloud MCP relay only when
        // cloud mode is enabled (§2.7 double gate: feature + cloud.enabled).
        #[cfg(feature = "cloud")]
        config.cloud.enabled.then(|| config.cloud.api_base.clone()),
        secrets.clone(),
    ));
    let approval_queue = Arc::new(ApprovalQueue::new());
    let ask_queue = Arc::new(AskQueue::new());
    let memory_manager_holder: Arc<RwLock<Option<Arc<MemoryManager>>>> =
        Arc::new(RwLock::new(None));

    let tool_registry = Arc::new(
        crate::gateway::create_default_tool_registry(
            crate::gateway::agent_spawn::ToolRegistryArgs {
                acp: acp.clone(),
                mcp_manager: mcp_manager.clone(),
                approval_queue: approval_queue.clone(),
                ask_queue: ask_queue.clone(),
                session_store: session_store.clone(),
                memory_manager: memory_manager_holder.clone(),
                capabilities: config.capabilities.clone(),
                audit_log: audit_log_dyn,
                content_filter: Some(Arc::new(ContentFilter::default())),
                search_config: config.search.clone(),
                #[cfg(feature = "cloud")]
                cloud_config: config.cloud.clone(),
                device_bridge,
                skills_manager,
                tool_hooks: shell_hooks.tool_hooks(),
                secrets: secrets.clone(),
            },
        )
        .await?,
    );

    let computer_adapter = init_computer_adapter(config, tool_registry.clone()).await;

    // Register ComputerTool with the adapter (or None if unavailable).
    if let Some(ref adapter) = computer_adapter {
        tool_registry.register_dynamic(Arc::new(crate::tools::computer::ComputerTool::new(Some(
            adapter.clone(),
        ))));
    } else {
        tool_registry.register_dynamic(Arc::new(crate::tools::computer::ComputerTool::new(None)));
    }

    // Register screen-state tools (screenshot + UI tree + optional OCR).
    #[cfg(feature = "vision")]
    let shared_ocr = crate::tools::screen_state::new_shared_ocr();
    tool_registry.register_dynamic(Arc::new(crate::tools::screen_state::ScreenStateTool::new(
        computer_adapter.clone(),
        #[cfg(feature = "vision")]
        shared_ocr.clone(),
        #[cfg(feature = "vision")]
        crate::tools::screen_state::new_shared_ui_detector(),
    )));
    #[cfg(feature = "vision")]
    tool_registry.register_dynamic(Arc::new(crate::tools::screen_state::ScreenOcrTool::new(
        computer_adapter.clone(),
        shared_ocr,
    )));
    #[cfg(feature = "vision")]
    tool_registry.register_dynamic(Arc::new(crate::tools::screen_state::ScreenUiDetectTool::new(
        computer_adapter.clone(),
        crate::tools::screen_state::new_shared_ui_detector(),
    )));

    // Create shared planner handle and register PlannerTool.
    let planner_handle: Arc<std::sync::RwLock<Option<Arc<crate::planner::GoalPlanner>>>> =
        Arc::new(std::sync::RwLock::new(None));
    tool_registry.register_dynamic(Arc::new(crate::tools::planner::PlannerTool::new(
        planner_handle.clone(),
    )));

    let channels = Arc::new(RwLock::new(HashMap::<String, Arc<dyn Channel>>::new()));
    let plugin_manager = init_plugin_manager(
        config,
        tool_registry.clone(),
        model_router,
        channels.clone(),
        task_registry,
    )
    .await?;
    let canvas_manager = Arc::new(CanvasManager::new());
    let channel_extensions = Arc::new(RwLock::new(ChannelExtensionRegistry::new()));

    Ok(ToolsInit {
        mcp_manager,
        connector_manager,
        mcp_event_rx,
        approval_queue,
        ask_queue,
        memory_manager_holder,
        tool_registry,
        computer_adapter,
        planner_handle,
        plugin_manager,
        canvas_manager,
        channels,
        channel_extensions,
    })
}
