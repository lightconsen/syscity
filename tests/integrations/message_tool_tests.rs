//! Message tool integration tests
//!
//! Tests MessageTool actions via a MockChannel to verify:
//! - Each action routes to the correct Channel method
//! - Capability checks reject unsupported actions
//! - Missing required arguments produce validation errors

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use syscity::channels::{Channel, ChannelCapabilities, ChatType, ConversationId, OutgoingMessage};
use syscity::core::models::Id;
use syscity::gateway::{GatewayConfig, GatewayState};
use syscity::tools::message::MessageTool;
use tokio::sync::{broadcast, mpsc, RwLock};

use super::*;

// ── Dummy pipelines (required by GatewayState but unused by these tests) ─────

struct DummyInboundPipeline;

#[async_trait]
impl syscity::inbound::InboundPipeline for DummyInboundPipeline {
    async fn process(
        &self,
        _message: syscity::channels::IncomingMessage,
    ) -> Option<syscity::inbound::RoutedMessage> {
        None
    }

    async fn flush(&self, _key: &str) -> Vec<syscity::inbound::RoutedMessage> {
        vec![]
    }
}

struct DummyOutboundPipeline;

#[async_trait]
impl syscity::outbound::OutboundPipeline for DummyOutboundPipeline {
    async fn process(
        &self,
        _ctx: syscity::outbound::OutboundContext,
    ) -> syscity::outbound::OutboundResult {
        syscity::outbound::OutboundResult {
            text: String::new(),
            canvas_update: None,
            sse_events: vec![],
            side_effects: vec![],
            session_id: String::new(),
            channel: String::new(),
        }
    }
}

// ── Test helper: construct a minimal GatewayState
// ─────────────────────────────

async fn make_test_state(config: GatewayConfig) -> GatewayState {
    let (event_tx, _) = broadcast::channel(1);
    let (inbound_entry_tx, _inbound_entry_rx) = mpsc::channel(1);
    let (routed_tx, _routed_rx) = mpsc::channel(1);

    let tmp = tempfile::tempdir().expect("create temp dir");
    let plugins_dir = tmp.path().join("plugins");
    let transcript_dir = tmp.path().join("transcripts");
    let artifact_dir = tmp.path().join("artifacts");
    let budget_dir = tmp.path().join("budget");
    let session_files_dir = tmp.path().join("session_files");

    let reply_dispatcher = Arc::new(syscity::outbound::ReplyDispatcher::new(
        syscity::outbound::ReplyDispatchConfig::default(),
    ));
    let side_effect_registry = Arc::new(syscity::outbound::SideEffectRegistry::new());
    let side_effect_executor =
        Arc::new(syscity::outbound::SideEffectExecutor::new(side_effect_registry));
    let sse_streamer = Arc::new(syscity::outbound::SseStreamer::new());

    let inbound_pipeline: Arc<dyn syscity::inbound::InboundPipeline> =
        Arc::new(DummyInboundPipeline);
    let outbound_pipeline: Arc<dyn syscity::outbound::OutboundPipeline> =
        Arc::new(DummyOutboundPipeline);

    let transcript_store = syscity::agent::TranscriptStore::new(transcript_dir);
    let _ = transcript_store.init().await;
    let artifact_store = syscity::agent::ArtifactStore::new(artifact_dir);
    let _ = artifact_store.init().await;
    let disk_budget = syscity::agent::DiskBudgetManager::new(budget_dir);
    let _ = disk_budget.init();
    let session_file_manager = syscity::agent::SessionFileManager::new(session_files_dir);
    let _ = session_file_manager.init().await;

    let skills_manager = Arc::new(RwLock::new(
        syscity::skills::SkillManager::new()
            .await
            .expect("skill manager"),
    ));

    GatewayState {
        config: Arc::new(RwLock::new(Arc::new(config))),
        start_time: std::time::Instant::now(),
        config_path: None,
        mcps_path: None,
        secrets: Arc::new(syscity::secrets::SecretStoreHandle::new()),
        auth: syscity::gateway::state::AuthState {
            manager: Arc::new(syscity::security::AuthManager::new()),
            pairing_store: Arc::new(syscity::security::pairing::PairingStore::new()),
            device_pairing_store: Arc::new(
                syscity::security::device_pairing::DevicePairingStore::new(),
            ),
            tailscale_authenticator: None,
            trusted_proxy_authenticator: None,
            rate_limiter: Arc::new(syscity::security::RateLimiter::new(100, 10.0)),
            multi_tier_rate_limiter: Arc::new(
                syscity::gateway::rate_limit::MultiTierRateLimiter::new(
                    syscity::gateway::rate_limit::MultiTierRateLimitConfig::default(),
                ),
            ),
            audit_log: Arc::new(syscity::security::persistent_audit::PersistentAuditLog::new()),
            command_gate: Arc::new(syscity::tools::command_gate::CommandGate::new()),
            mention_gate: Arc::new(syscity::security::mention_gate::MentionGate::new(
                syscity::security::mention_gate::MentionPolicy::Allow,
            )),
        },
        agents: syscity::gateway::state::AgentState {
            agents: Arc::new(RwLock::new(HashMap::new())),
            pending_spawns: Arc::new(std::sync::Mutex::new(HashSet::new())),
            router: Arc::new(syscity::inbound::AgentRouter::new(
                syscity::inbound::AgentRouterConfig::default(),
            )),
            registry: Arc::new(RwLock::new(syscity::agent::AgentRegistry::new())),
            manager: Arc::new(RwLock::new(syscity::agent::SessionManager::new())),
            group_manager: Arc::new(RwLock::new(syscity::agent::GroupSessionManager::new())),
            store: None,
            message_buffer: Arc::new(RwLock::new(HashMap::new())),
            route_resolver: Arc::new(syscity::agent::RouteResolver::new("default")),
            cost_guard: syscity::agent::CostGuard::new(0, 0),
            repair_state: Arc::new(syscity::gateway::RepairState::new()),
            acp: Arc::new(syscity::acp::AcpControlPlane::new(10)),
            goal_cancellers: Arc::new(RwLock::new(HashMap::new())),
        },
        channels: syscity::gateway::state::ChannelState {
            channels: Arc::new(RwLock::new(HashMap::new())),
            extensions: Arc::new(RwLock::new(syscity::channels::ChannelExtensionRegistry::new())),
            reply_dispatcher,
            snapshot_store: None,
            health_monitor: None,
            acp_bridge: None,
            session_channels: Arc::new(RwLock::new(HashMap::new())),
            webhook_sessions: Arc::new(RwLock::new(HashMap::new())),
        },
        memory: syscity::gateway::state::MemoryState {
            vector: tokio::sync::RwLock::new(None),
            session_search: tokio::sync::RwLock::new(None),
            manager: Arc::new(RwLock::new(None)),
            dream_scheduler: tokio::sync::RwLock::new(None),
            dream_metrics: Arc::new(syscity::memory::DreamMetrics::default()),
            standing_order_manager: tokio::sync::RwLock::new(None),
            kb_manager: tokio::sync::RwLock::new(None),
        },
        tools: syscity::gateway::state::ToolState {
            registry: Arc::new(syscity::tools::ToolRegistry::new()),
            mcp_manager: Arc::new(McpManager::default()),
            connector_manager: Arc::new(syscity::mcp::ConnectorManager::new(
                std::env::temp_dir()
                    .join(format!("syscity_msg_tool_test_{}", uuid::Uuid::new_v4())),
                Arc::new(McpManager::default()),
                Arc::new(syscity::skills::SkillStorage::with_user_dir(
                    std::env::temp_dir()
                        .join(format!("syscity_msg_tool_test_sk_{}", uuid::Uuid::new_v4())),
                )),
                #[cfg(feature = "cloud")]
                None,
                Arc::new(syscity::secrets::SecretStoreHandle::new()),
            )),
            approval_queue: Arc::new(syscity::tools::approval::ApprovalQueue::new()),
            ask_queue: Arc::new(syscity::tools::ask_user::AskQueue::new()),
            skills_manager,
            canvas_manager: Arc::new(syscity::canvas::CanvasManager::new()),
            computer_adapter: Arc::new(tokio::sync::RwLock::new(None)),
            planner_handle: Arc::new(std::sync::RwLock::new(None)),
        },
        pipelines: syscity::gateway::state::PipelineState {
            inbound: inbound_pipeline,
            outbound: outbound_pipeline,
            side_effect_executor,
            sse_streamer,
            routed_tx,
            inbound_entry: inbound_entry_tx,
        },
        events: syscity::gateway::state::EventState {
            tx: event_tx,
            log_tx: tokio::sync::broadcast::channel(1000).0,
            hook_registry: Arc::new(syscity::gateway::hooks::EventHookRegistry::new()),
        },
        infra: syscity::gateway::state::InfraState {
            storage: Arc::new(RwLock::new(syscity::adapters::InMemoryStorage::new())),
            runtime_settings: Arc::new(RwLock::new(HashMap::new())),
            transcript_store: Arc::new(transcript_store),
            artifact_store: Arc::new(artifact_store),
            disk_budget: Arc::new(disk_budget),
            session_file_manager: Arc::new(session_file_manager),
            hot_reload: tokio::sync::RwLock::new(None),
            plugin_manager: Arc::new(
                syscity::plugins::PluginManager::new(plugins_dir)
                    .await
                    .expect("plugin manager"),
            ),
            model_router: Arc::new(syscity::model_router::ModelRouter::new(
                syscity::model_router::ModelRouterConfig::default(),
            )),
            shell_hooks: syscity::hooks::ShellHookBridge::empty(),
            feedback_store: None,
            pending_badcase_store: None,
            decision_trace_store: None,
            sample_store: None,
            optimizer: Arc::new(syscity::eval::OptimizerRuntime::default()),
            #[cfg(feature = "browser")]
            browser_bridge: tokio::sync::RwLock::new(None),
        },
        sdk: syscity::gateway::state::SdkState {
            provider_sdk: Arc::new(RwLock::new(syscity::providers::ProviderSdk::new())),
            tool_sdk: Arc::new(RwLock::new(syscity::tools::ToolSdk::new())),
        },
        scheduler: syscity::gateway::state::SchedulerState {
            task_scheduler: tokio::sync::RwLock::new(None),
            heartbeat_wake_tx: tokio::sync::RwLock::new(None),
            heartbeat_event_tx: tokio::sync::RwLock::new(None),
            cron_scheduler: tokio::sync::RwLock::new(None),
        },
        device: syscity::gateway::state::DeviceState {
            bridge: tokio::sync::RwLock::new(None),
        },
        task_registry: Arc::new(syscity::gateway::task_registry::TaskRegistry::new()),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
        update: syscity::gateway::state::UpdateState::new(),
        // Tests control `embedded` explicitly; production captures the env.
        embedded: false,
    }
}

// ── MockChannel ──────────────────────────────────────────────────────────────

/// A mock channel that records every method call for later verification.
#[derive(Debug, Clone)]
struct MockChannel {
    name: String,
    caps: ChannelCapabilities,
    calls: Arc<RwLock<Vec<String>>>,
}

impl MockChannel {
    fn new(name: impl Into<String>, caps: ChannelCapabilities) -> Self {
        Self {
            name: name.into(),
            caps,
            calls: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Drain recorded calls (for assertions).
    async fn drain_calls(&self) -> Vec<String> {
        let mut guard = self.calls.write().await;
        std::mem::take(&mut *guard)
    }
}

#[async_trait]
impl Channel for MockChannel {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> ChannelCapabilities {
        self.caps.clone()
    }

    async fn start(&self) -> syscity::Result<()> {
        self.calls.write().await.push("start".to_string());
        Ok(())
    }

    async fn stop(&self) -> syscity::Result<()> {
        self.calls.write().await.push("stop".to_string());
        Ok(())
    }

    async fn send(&self, msg: OutgoingMessage) -> syscity::Result<Id> {
        let reply = msg
            .reply_to
            .map(|r| format!(":reply_to={}", r))
            .unwrap_or_default();
        self.calls
            .write()
            .await
            .push(format!("send:{}:{}{}", msg.conversation_id, msg.content, reply));
        Ok(Id::new())
    }

    async fn send_typing(&self, conversation_id: &ConversationId) -> syscity::Result<()> {
        self.calls
            .write()
            .await
            .push(format!("typing:{}", conversation_id));
        Ok(())
    }

    async fn edit_message(&self, message_id: Id, new_content: String) -> syscity::Result<()> {
        self.calls
            .write()
            .await
            .push(format!("edit:{}:{}", message_id, new_content));
        Ok(())
    }

    async fn delete_message(&self, message_id: Id) -> syscity::Result<()> {
        self.calls
            .write()
            .await
            .push(format!("delete:{}", message_id));
        Ok(())
    }

    async fn health_check(&self) -> syscity::Result<bool> {
        Ok(true)
    }

    async fn add_reaction(&self, message_id: Id, emoji: String) -> syscity::Result<()> {
        self.calls
            .write()
            .await
            .push(format!("react:{}:{}", message_id, emoji));
        Ok(())
    }

    async fn remove_reaction(&self, message_id: Id, emoji: String) -> syscity::Result<()> {
        self.calls
            .write()
            .await
            .push(format!("unreact:{}:{}", message_id, emoji));
        Ok(())
    }

    async fn pin_message(&self, message_id: Id) -> syscity::Result<()> {
        self.calls.write().await.push(format!("pin:{}", message_id));
        Ok(())
    }

    async fn unpin_message(&self, message_id: Id) -> syscity::Result<()> {
        self.calls
            .write()
            .await
            .push(format!("unpin:{}", message_id));
        Ok(())
    }

    async fn create_thread(
        &self,
        message_id: Id,
        title: Option<String>,
    ) -> syscity::Result<ConversationId> {
        self.calls
            .write()
            .await
            .push(format!("thread:{}:{:?}", message_id, title));
        Ok(ConversationId::new("thread-123"))
    }

    async fn send_poll(
        &self,
        conversation_id: ConversationId,
        question: String,
        options: Vec<String>,
    ) -> syscity::Result<Id> {
        self.calls
            .write()
            .await
            .push(format!("poll:{}:{}:{:?}", conversation_id, question, options));
        Ok(Id::new())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn full_caps() -> ChannelCapabilities {
    ChannelCapabilities {
        chat_types: vec![ChatType::Direct, ChatType::Group],
        supports_formatting: true,
        supports_attachments: true,
        supports_images: true,
        supports_threads: true,
        supports_typing: true,
        supports_buttons: true,
        supports_commands: true,
        supports_reactions: true,
        supports_edit: true,
        supports_unsend: true,
        supports_effects: false,
    }
}

fn restricted_caps() -> ChannelCapabilities {
    ChannelCapabilities {
        supports_edit: false,
        supports_unsend: false,
        supports_reactions: false,
        supports_threads: false,
        supports_typing: false,
        ..full_caps()
    }
}

async fn setup_with_channel(channel: Arc<dyn Channel>) -> (MessageTool, String) {
    let config = GatewayConfig::default();
    let state: GatewayState = make_test_state(config).await;
    state
        .channels
        .channels
        .write()
        .await
        .insert("mock".to_string(), channel);
    let tool = MessageTool::new(Arc::new(state));
    (tool, "mock".to_string())
}

// ── Action routing tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn send_action_routes_to_channel_send() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;

    let result = tool
        .execute(
            json!({
                "action": "send",
                "channel": ch,
                "conversation_id": "conv1",
                "content": "hello world"
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(result.success, "send should succeed: {:?}", result.error);
    let calls = mock.drain_calls().await;
    assert_eq!(calls, vec!["send:conv1:hello world"]);
}

#[tokio::test]
async fn reply_action_routes_to_channel_send_with_reply_to() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;
    let msg_id = Id::new().to_string();

    let result = tool
        .execute(
            json!({
                "action": "reply",
                "channel": ch,
                "conversation_id": "conv1",
                "content": "reply text",
                "message_id": msg_id
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(result.success, "reply should succeed: {:?}", result.error);
    let calls = mock.drain_calls().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with("send:conv1:reply text:reply_to="));
}

#[tokio::test]
async fn edit_action_routes_to_channel_edit() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;
    let msg_id = Id::new().to_string();

    let result = tool
        .execute(
            json!({
                "action": "edit",
                "channel": ch,
                "conversation_id": "conv1",
                "content": "edited",
                "message_id": msg_id
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(result.success, "edit should succeed: {:?}", result.error);
    let calls = mock.drain_calls().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with("edit:"));
    assert!(calls[0].ends_with(":edited"));
}

#[tokio::test]
async fn delete_action_routes_to_channel_delete() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;
    let msg_id = Id::new().to_string();

    let result = tool
        .execute(
            json!({
                "action": "delete",
                "channel": ch,
                "conversation_id": "conv1",
                "message_id": msg_id
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(result.success, "delete should succeed: {:?}", result.error);
    let calls = mock.drain_calls().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with("delete:"));
}

#[tokio::test]
async fn typing_action_routes_to_channel_send_typing() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;

    let result = tool
        .execute(
            json!({
                "action": "typing",
                "channel": ch,
                "conversation_id": "conv1"
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(result.success, "typing should succeed: {:?}", result.error);
    let calls = mock.drain_calls().await;
    assert_eq!(calls, vec!["typing:conv1"]);
}

#[tokio::test]
async fn react_action_routes_to_channel_add_reaction() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;
    let msg_id = Id::new().to_string();

    let result = tool
        .execute(
            json!({
                "action": "react",
                "channel": ch,
                "conversation_id": "conv1",
                "message_id": msg_id,
                "emoji": "👍"
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(result.success, "react should succeed: {:?}", result.error);
    let calls = mock.drain_calls().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with("react:"));
    assert!(calls[0].ends_with(":👍"));
}

#[tokio::test]
async fn unreact_action_routes_to_channel_remove_reaction() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;
    let msg_id = Id::new().to_string();

    let result = tool
        .execute(
            json!({
                "action": "unreact",
                "channel": ch,
                "conversation_id": "conv1",
                "message_id": msg_id,
                "emoji": "👍"
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(result.success, "unreact should succeed: {:?}", result.error);
    let calls = mock.drain_calls().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with("unreact:"));
}

#[tokio::test]
async fn pin_action_routes_to_channel_pin() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;
    let msg_id = Id::new().to_string();

    let result = tool
        .execute(
            json!({
                "action": "pin",
                "channel": ch,
                "conversation_id": "conv1",
                "message_id": msg_id
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(result.success, "pin should succeed: {:?}", result.error);
    let calls = mock.drain_calls().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with("pin:"));
}

#[tokio::test]
async fn unpin_action_routes_to_channel_unpin() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;
    let msg_id = Id::new().to_string();

    let result = tool
        .execute(
            json!({
                "action": "unpin",
                "channel": ch,
                "conversation_id": "conv1",
                "message_id": msg_id
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(result.success, "unpin should succeed: {:?}", result.error);
    let calls = mock.drain_calls().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with("unpin:"));
}

#[tokio::test]
async fn thread_create_action_routes_to_channel_create_thread() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;
    let msg_id = Id::new().to_string();

    let result = tool
        .execute(
            json!({
                "action": "thread_create",
                "channel": ch,
                "conversation_id": "conv1",
                "message_id": msg_id,
                "thread_title": "Discussion"
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(result.success, "thread_create should succeed: {:?}", result.error);
    let calls = mock.drain_calls().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with("thread:"));
    assert!(calls[0].contains("Discussion"));
}

#[tokio::test]
async fn poll_action_routes_to_channel_send_poll() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;

    let result = tool
        .execute(
            json!({
                "action": "poll",
                "channel": ch,
                "conversation_id": "conv1",
                "poll_question": "Q?",
                "poll_options": ["A", "B"]
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(result.success, "poll should succeed: {:?}", result.error);
    let calls = mock.drain_calls().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with("poll:conv1:Q?:"));
}

// ── Capability rejection tests ───────────────────────────────────────────────

#[tokio::test]
async fn edit_rejected_when_channel_does_not_support_edit() {
    let mock = Arc::new(MockChannel::new("mock", restricted_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;
    let msg_id = Id::new().to_string();

    let result = tool
        .execute(
            json!({
                "action": "edit",
                "channel": ch,
                "conversation_id": "conv1",
                "content": "edited",
                "message_id": msg_id
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(!result.success, "edit should be rejected");
    assert!(result
        .error
        .as_ref()
        .unwrap()
        .contains("does not support message editing"));
    let calls = mock.drain_calls().await;
    assert!(calls.is_empty(), "no channel method should be called");
}

#[tokio::test]
async fn delete_rejected_when_channel_does_not_support_unsend() {
    let mock = Arc::new(MockChannel::new("mock", restricted_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;
    let msg_id = Id::new().to_string();

    let result = tool
        .execute(
            json!({
                "action": "delete",
                "channel": ch,
                "conversation_id": "conv1",
                "message_id": msg_id
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(!result.success, "delete should be rejected");
    assert!(result
        .error
        .as_ref()
        .unwrap()
        .contains("does not support message deletion"));
    let calls = mock.drain_calls().await;
    assert!(calls.is_empty());
}

#[tokio::test]
async fn typing_rejected_when_channel_does_not_support_typing() {
    let mock = Arc::new(MockChannel::new("mock", restricted_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;

    let result = tool
        .execute(
            json!({
                "action": "typing",
                "channel": ch,
                "conversation_id": "conv1"
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(!result.success, "typing should be rejected");
    assert!(result
        .error
        .as_ref()
        .unwrap()
        .contains("does not support typing indicators"));
    let calls = mock.drain_calls().await;
    assert!(calls.is_empty());
}

#[tokio::test]
async fn react_rejected_when_channel_does_not_support_reactions() {
    let mock = Arc::new(MockChannel::new("mock", restricted_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;
    let msg_id = Id::new().to_string();

    let result = tool
        .execute(
            json!({
                "action": "react",
                "channel": ch,
                "conversation_id": "conv1",
                "message_id": msg_id,
                "emoji": "👍"
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(!result.success, "react should be rejected");
    assert!(result
        .error
        .as_ref()
        .unwrap()
        .contains("does not support reactions"));
    let calls = mock.drain_calls().await;
    assert!(calls.is_empty());
}

#[tokio::test]
async fn thread_create_rejected_when_channel_does_not_support_threads() {
    let mock = Arc::new(MockChannel::new("mock", restricted_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;
    let msg_id = Id::new().to_string();

    let result = tool
        .execute(
            json!({
                "action": "thread_create",
                "channel": ch,
                "conversation_id": "conv1",
                "message_id": msg_id
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(!result.success, "thread_create should be rejected");
    assert!(result
        .error
        .as_ref()
        .unwrap()
        .contains("does not support threads"));
    let calls = mock.drain_calls().await;
    assert!(calls.is_empty());
}

// ── Validation error tests ───────────────────────────────────────────────────

#[tokio::test]
async fn unknown_channel_returns_error() {
    let config = GatewayConfig::default();
    let state = Arc::new(make_test_state(config).await);
    let tool = MessageTool::new(state);

    let result = tool
        .execute(
            json!({
                "action": "send",
                "channel": "nonexistent",
                "conversation_id": "conv1",
                "content": "hello"
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(!result.success);
    assert!(result.error.as_ref().unwrap().contains("not found"));
}

#[tokio::test]
async fn send_without_content_returns_error() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;

    let result = tool
        .execute(
            json!({
                "action": "send",
                "channel": ch,
                "conversation_id": "conv1"
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(!result.success);
    assert!(result
        .error
        .as_ref()
        .unwrap()
        .contains("content is required"));
    let calls = mock.drain_calls().await;
    assert!(calls.is_empty());
}

#[tokio::test]
async fn reply_without_message_id_returns_error() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;

    let result = tool
        .execute(
            json!({
                "action": "reply",
                "channel": ch,
                "conversation_id": "conv1",
                "content": "reply"
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(!result.success);
    assert!(result
        .error
        .as_ref()
        .unwrap()
        .contains("message_id is required"));
    let calls = mock.drain_calls().await;
    assert!(calls.is_empty());
}

#[tokio::test]
async fn react_without_emoji_returns_error() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;
    let msg_id = Id::new().to_string();

    let result = tool
        .execute(
            json!({
                "action": "react",
                "channel": ch,
                "conversation_id": "conv1",
                "message_id": msg_id
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(!result.success);
    assert!(result.error.as_ref().unwrap().contains("emoji is required"));
    let calls = mock.drain_calls().await;
    assert!(calls.is_empty());
}

#[tokio::test]
async fn poll_without_question_returns_error() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;

    let result = tool
        .execute(
            json!({
                "action": "poll",
                "channel": ch,
                "conversation_id": "conv1",
                "poll_options": ["A"]
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(!result.success);
    assert!(result
        .error
        .as_ref()
        .unwrap()
        .contains("poll_question is required"));
    let calls = mock.drain_calls().await;
    assert!(calls.is_empty());
}

#[tokio::test]
async fn poll_without_options_returns_error() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;

    let result = tool
        .execute(
            json!({
                "action": "poll",
                "channel": ch,
                "conversation_id": "conv1",
                "poll_question": "Q?"
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(!result.success);
    assert!(result
        .error
        .as_ref()
        .unwrap()
        .contains("poll_options is required"));
    let calls = mock.drain_calls().await;
    assert!(calls.is_empty());
}

#[tokio::test]
async fn unknown_action_returns_error() {
    let mock = Arc::new(MockChannel::new("mock", full_caps()));
    let (tool, ch) = setup_with_channel(mock.clone()).await;

    let result = tool
        .execute(
            json!({
                "action": "dance",
                "channel": ch,
                "conversation_id": "conv1"
            }),
            &test_context(),
        )
        .await
        .expect("tool execute should not error");

    assert!(!result.success);
    assert!(result
        .error
        .as_ref()
        .unwrap()
        .contains("Unknown message action"));
    let calls = mock.drain_calls().await;
    assert!(calls.is_empty());
}
