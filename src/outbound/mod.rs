//! Outbound Pipeline for Syscity
//!
//! The outbound pipeline handles everything that happens *after* the agent
//! produces a response.
//!
//! ```text
//! Agent Output
//! -> Canvas (render dynamic UI)
//! -> SSE (stream to connected clients)
//! -> Reply Dispatcher (route to correct channel)
//! -> Side Effects (memory, cron, webhooks, …)
//! ```
// INVARIANTS-NONE: dispatch plumbing toward channel providers; no local durable state.

use std::sync::Arc;

pub mod reply_dispatcher;
pub mod side_effects;
pub mod sse;
pub mod stage;

pub use reply_dispatcher::{ReplyDispatchConfig, ReplyDispatcher};
pub use side_effects::{SideEffect, SideEffectContext, SideEffectExecutor, SideEffectRegistry};
pub use sse::{SseEvent, SseStreamer};

use self::stage::{default_outbound_stages_from_arcs, run_outbound_stages, OutboundStageContext};
use crate::channels::reply_prefix::ReplyPrefixEngine;

/// A fully-processed outbound result ready for delivery.
#[derive(Debug, Clone)]
pub struct OutboundResult {
    /// The final text to send to the user.
    pub text: String,
    /// Optional canvas update to push.
    pub canvas_update: Option<crate::canvas::CanvasUpdate>,
    /// SSE events that should be streamed.
    pub sse_events: Vec<SseEvent>,
    /// Side effects to execute after delivery.
    pub side_effects: Vec<SideEffect>,
    /// Target session ID.
    pub session_id: String,
    /// Target channel.
    pub channel: String,
}

/// The outbound pipeline trait.
#[async_trait::async_trait]
pub trait OutboundPipeline: Send + Sync {
    /// Process an agent output through the full outbound pipeline.
    async fn process(&self, ctx: OutboundContext) -> OutboundResult;
}

/// Context passed through the outbound pipeline.
#[derive(Debug, Clone)]
pub struct OutboundContext {
    pub session_id: String,
    /// Where the reply is addressed, when that is not the session.
    ///
    /// A webhook channel's session id is a UUID *this gateway minted*, so
    /// addressing a reply to it means addressing it to nothing the platform
    /// recognises — LINE would push to a UUID, Twilio would text one, Google
    /// Chat would post into a space that does not exist. The inbound handler
    /// knows the real platform id (it is what it recorded in
    /// `session_channels`), so it travels here and the dispatch stage prefers
    /// it, falling back to the session id for the paths that have no platform
    /// id to give.
    pub conversation_id: Option<String>,
    pub channel: String,
    pub agent_id: String,
    pub raw_output: String,
    pub tool_calls: Vec<crate::providers::ToolCall>,
    /// Optional token usage statistics.
    pub usage: Option<crate::providers::Usage>,
    /// Side effects to execute after the reply is dispatched.
    pub side_effects: Vec<SideEffect>,
    /// LLM model name (e.g. "claude-sonnet-4-6"), if known.
    pub model_name: Option<String>,
    /// LLM provider name (e.g. "anthropic"), if known.
    pub model_provider: Option<String>,
    /// Optional reasoning/thinking content from the LLM.
    pub reasoning_content: Option<String>,
}

/// Default outbound pipeline implementation.
///
/// Wires all stages together: canvas -> sse -> reply -> side effects.
pub struct DefaultOutboundPipeline {
    reply_dispatcher: Arc<ReplyDispatcher>,
    side_effects: Arc<SideEffectExecutor>,
    sse: Option<Arc<SseStreamer>>,
    /// Optional reply prefix engine for prepending model info / metadata.
    reply_prefix_engine: Option<ReplyPrefixEngine>,
}

impl DefaultOutboundPipeline {
    pub fn new(
        reply_dispatcher: Arc<ReplyDispatcher>,
        side_effects: Arc<SideEffectExecutor>,
        sse: Option<Arc<SseStreamer>>,
    ) -> Self {
        Self {
            reply_dispatcher,
            side_effects,
            sse,
            reply_prefix_engine: None,
        }
    }

    /// Attach a reply prefix engine to prepend model info / metadata
    /// before dispatching messages.
    pub fn with_reply_prefix_engine(mut self, engine: ReplyPrefixEngine) -> Self {
        self.reply_prefix_engine = Some(engine);
        self
    }
}

#[async_trait::async_trait]
impl OutboundPipeline for DefaultOutboundPipeline {
    async fn process(&self, ctx: OutboundContext) -> OutboundResult {
        let stages = default_outbound_stages_from_arcs(
            self.sse.clone(),
            self.reply_prefix_engine.clone(),
            self.reply_dispatcher.clone(),
            self.side_effects.clone(),
        );

        let mut stage_ctx = OutboundStageContext::new(ctx);
        run_outbound_stages(&stages, &mut stage_ctx).await
    }
}
