//! Inbound/outbound pipeline initialization.
//!
//! Wires together the message routing pipeline: agent router, reply dispatcher,
//! inbound debouncer, media understanding, outbound pipeline, side-effect
//! executor, and SSE streamer.

use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::warn;

use crate::gateway::GatewayConfig;
use crate::inbound::{
    AgentRouter, AutoReplyDispatch, AutoReplyDispatchConfig, DefaultInboundPipeline,
    InboundDebouncer, InboundDebouncerConfig, MediaUnderstandingPipeline, QueueModeResolver,
};
use crate::model_router::ModelRouter;
use crate::outbound::{
    DefaultOutboundPipeline, OutboundPipeline, ReplyDispatchConfig, ReplyDispatcher,
    SideEffectExecutor, SideEffectRegistry, SseStreamer,
};

/// Pipeline subsystem initialization result.
pub struct PipelinesInit {
    pub agent_router: Arc<AgentRouter>,
    pub reply_dispatcher: Arc<ReplyDispatcher>,
    pub inbound_pipeline: Arc<dyn crate::inbound::InboundPipeline>,
    pub outbound_pipeline: Arc<dyn OutboundPipeline>,
    pub side_effect_executor: Arc<SideEffectExecutor>,
    pub sse_streamer: Arc<SseStreamer>,
    pub routed_tx: mpsc::Sender<crate::inbound::RoutedMessage>,
}

/// Initialize inbound and outbound pipelines.
#[allow(clippy::type_complexity)]
pub async fn init_pipelines(
    config: &GatewayConfig,
    sqlite_pool: Option<&sqlx::SqlitePool>,
    model_router: Arc<ModelRouter>,
    routed_tx: mpsc::Sender<crate::inbound::RoutedMessage>,
    denied_tx: mpsc::Sender<crate::inbound::DeniedMessage>,
) -> crate::Result<PipelinesInit> {
    let agent_router = if let Some(pool) = sqlite_pool {
        let binding_store = Arc::new(
            crate::inbound::SqliteBindingStore::new(pool.clone())
                .await
                .map_err(|e| crate::error::SyscityError::Storage {
                    context: "Failed to create binding store".to_string(),
                    details: e.to_string(),
                })?,
        );
        let router = AgentRouter::new(crate::inbound::AgentRouterConfig::default())
            .with_binding_store(binding_store);
        if let Err(e) = router.load_bindings().await {
            warn!("Failed to load agent router bindings: {}", e);
        }
        Arc::new(router)
    } else {
        Arc::new(AgentRouter::new(crate::inbound::AgentRouterConfig::default()))
    };

    // Who may use which agent, from `agent_overrides.<id>.allowed_users`.
    // Seeded here (not lazily on first message) so a configured restriction
    // holds from the first turn, exactly like the per-agent cost caps.
    for (id, users) in config.per_agent_allowed_users() {
        agent_router.set_agent_principals(&id, users).await;
    }

    let reply_dispatcher = Arc::new(ReplyDispatcher::new(ReplyDispatchConfig::default()));
    let (debounce_flush_tx, debounce_flush_rx) = mpsc::channel(256);
    let debouncer = InboundDebouncer::new(InboundDebouncerConfig::default(), debounce_flush_tx);

    let inbound_concrete = Arc::new(
        DefaultInboundPipeline::new(
            debouncer.clone(),
            MediaUnderstandingPipeline::new().with_model_router(Arc::clone(&model_router)),
            AutoReplyDispatch::new(AutoReplyDispatchConfig::default()),
            QueueModeResolver::new(),
            Arc::clone(&agent_router),
            routed_tx.clone(),
            debounce_flush_rx,
        )
        .with_denied_sink(denied_tx),
    );
    inbound_concrete.clone().start();
    let inbound_pipeline: Arc<dyn crate::inbound::InboundPipeline> = inbound_concrete;

    let side_effect_registry = Arc::new(SideEffectRegistry::new());
    let side_effect_executor = Arc::new(SideEffectExecutor::new(side_effect_registry));
    let sse_streamer = Arc::new(SseStreamer::new());
    let outbound_pipeline: Arc<dyn OutboundPipeline> = Arc::new(DefaultOutboundPipeline::new(
        reply_dispatcher.clone(),
        side_effect_executor.clone(),
        Some(sse_streamer.clone()),
    ));

    Ok(PipelinesInit {
        agent_router,
        reply_dispatcher,
        inbound_pipeline,
        outbound_pipeline,
        side_effect_executor,
        sse_streamer,
        routed_tx,
    })
}
