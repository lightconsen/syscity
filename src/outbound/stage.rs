//! Pluggable outbound pipeline stages.
//!
//! Defines the [`OutboundStage`] trait and shared context, plus stage wrappers
//! for each existing pipeline step.
//!
//! The default stage list mirrors the current hard-coded order:
//! `Canvas → SSE → ReplyPrefix → Dispatch → SideEffects`

use std::sync::Arc;

use async_trait::async_trait;

use super::{
    OutboundContext, OutboundResult, ReplyDispatcher, SideEffectExecutor, SseEvent, SseStreamer,
};
use crate::canvas::CanvasComponent;
use crate::canvas::CanvasUpdate;
use crate::channels::reply_prefix::{ReplyPrefixEngine, TemplateContext};

// ── Actions ──────────────────────────────────────────────────────────────────

/// What the pipeline should do after this stage returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboundStageAction {
    /// Continue to the next stage normally.
    Continue,
    /// Skip dispatch to the channel (result is still returned).
    SkipDispatch,
}

// ── Context ──────────────────────────────────────────────────────────────────

/// Shared mutable state threaded through all outbound stages.
///
/// Holds both the original [`OutboundContext`] (input) and the accumulating
/// [`OutboundResult`] (output).
#[derive(Debug, Clone)]
pub struct OutboundStageContext {
    /// Original pipeline input (immutable after construction).
    pub input: OutboundContext,
    /// Accumulating pipeline output.
    pub result: OutboundResult,
    /// Whether the final dispatch stage should run.
    pub skip_dispatch: bool,
}

impl OutboundStageContext {
    pub fn new(input: OutboundContext) -> Self {
        let result = OutboundResult {
            text: input.raw_output.clone(),
            canvas_update: None,
            sse_events: Vec::new(),
            side_effects: input.side_effects.clone(),
            session_id: input.session_id.clone(),
            channel: input.channel.clone(),
        };
        Self {
            input,
            result,
            skip_dispatch: false,
        }
    }
}

// ── Trait ────────────────────────────────────────────────────────────────────

/// A single stage in the outbound pipeline.
///
/// Stages are executed in order. Each stage inspects or mutates
/// [`OutboundStageContext`] and returns an [`OutboundStageAction`] to
/// indicate whether the pipeline should continue.
#[async_trait]
pub trait OutboundStage: Send + Sync {
    /// Unique name for this stage (used for logging and ordering).
    fn name(&self) -> &str;

    /// Process the outbound data. Return [`OutboundStageAction::Continue`] to
    /// proceed, or [`OutboundStageAction::SkipDispatch`] to skip channel
    /// dispatch.
    async fn process(&self, ctx: &mut OutboundStageContext) -> OutboundStageAction;
}

// ── Built-in stage wrappers ──────────────────────────────────────────────────

/// Canvas rendering stage – detects A2UI components in agent output.
pub struct CanvasStage;

#[async_trait]
impl OutboundStage for CanvasStage {
    fn name(&self) -> &str {
        "canvas"
    }

    async fn process(&self, ctx: &mut OutboundStageContext) -> OutboundStageAction {
        let text = ctx.result.text.trim();
        if text.starts_with('{') {
            ctx.result.canvas_update =
                serde_json::from_str::<CanvasComponent>(text)
                    .ok()
                    .map(|component| CanvasUpdate::Init {
                        canvas_id: ctx.input.session_id.clone(),
                        root: component,
                    });
        }
        OutboundStageAction::Continue
    }
}

/// SSE streaming stage – emits tool call and completion events.
pub struct SseStage {
    sse: Arc<SseStreamer>,
}

impl SseStage {
    pub fn new(sse: SseStreamer) -> Self {
        Self { sse: Arc::new(sse) }
    }

    /// Create from an already-`Arc`-wrapped streamer.
    pub fn from_arc(sse: Arc<SseStreamer>) -> Self {
        Self { sse }
    }
}

#[async_trait]
impl OutboundStage for SseStage {
    fn name(&self) -> &str {
        "sse"
    }

    async fn process(&self, ctx: &mut OutboundStageContext) -> OutboundStageAction {
        for tc in &ctx.input.tool_calls {
            let start_evt = SseEvent::ToolStart { name: tc.function.name.clone() };
            self.sse
                .send(&ctx.input.session_id, start_evt.clone())
                .await;
            ctx.result.sse_events.push(start_evt);

            let end_evt = SseEvent::ToolEnd {
                name: tc.function.name.clone(),
                result: serde_json::Value::Null,
            };
            self.sse.send(&ctx.input.session_id, end_evt.clone()).await;
            ctx.result.sse_events.push(end_evt);
        }
        let done_evt = SseEvent::Done;
        self.sse.send(&ctx.input.session_id, done_evt.clone()).await;
        ctx.result.sse_events.push(done_evt);
        OutboundStageAction::Continue
    }
}

/// Reply prefix stage – prepends model info / metadata to the text.
pub struct ReplyPrefixStage {
    engine: ReplyPrefixEngine,
}

impl ReplyPrefixStage {
    pub fn new(engine: ReplyPrefixEngine) -> Self {
        Self { engine }
    }
}

#[async_trait]
impl OutboundStage for ReplyPrefixStage {
    fn name(&self) -> &str {
        "reply_prefix"
    }

    async fn process(&self, ctx: &mut OutboundStageContext) -> OutboundStageAction {
        let mut template_ctx = TemplateContext::new()
            .with_session(&ctx.input.session_id)
            .with_channel(&ctx.input.channel);

        // Enrich with available metadata
        if let Some(ref model_name) = ctx.input.model_name {
            template_ctx = template_ctx.with_model(model_name);
        }
        if let Some(ref provider) = ctx.input.model_provider {
            template_ctx = template_ctx.with_provider(provider);
        }
        if let Some(ref usage) = ctx.input.usage {
            template_ctx = template_ctx
                .with_custom("prompt_tokens", usage.prompt_tokens.to_string())
                .with_custom("completion_tokens", usage.completion_tokens.to_string())
                .with_custom("total_tokens", usage.total_tokens.to_string());
        }

        let prefixed = self
            .engine
            .apply_async(&ctx.result.text, &template_ctx, Some(&ctx.input.channel))
            .await;
        ctx.result.text = prefixed;
        OutboundStageAction::Continue
    }
}

/// Channel dispatch stage – routes the final message to the correct channel.
pub struct DispatchStage {
    dispatcher: Arc<ReplyDispatcher>,
}

impl DispatchStage {
    pub fn new(dispatcher: ReplyDispatcher) -> Self {
        Self {
            dispatcher: Arc::new(dispatcher),
        }
    }

    /// Create from an already-`Arc`-wrapped dispatcher.
    pub fn from_arc(dispatcher: Arc<ReplyDispatcher>) -> Self {
        Self { dispatcher }
    }
}

#[async_trait]
impl OutboundStage for DispatchStage {
    fn name(&self) -> &str {
        "dispatch"
    }

    async fn process(&self, ctx: &mut OutboundStageContext) -> OutboundStageAction {
        if ctx.skip_dispatch {
            return OutboundStageAction::SkipDispatch;
        }

        let outbound_msg = crate::channels::OutgoingMessage {
            // The platform's own id when the inbound path knew one — see
            // `OutboundContext::conversation_id`. The session id is the
            // fallback, and for the built-in surfaces it is the right answer.
            conversation_id: crate::channels::ConversationId::new(
                ctx.input
                    .conversation_id
                    .as_deref()
                    .unwrap_or(&ctx.input.session_id),
            ),
            content: ctx.result.text.clone(),
            reasoning_content: ctx.input.reasoning_content.clone(),
            tool_calls: None,
            formatted_content: None,
            attachments: vec![],
            reply_to: None,
            options: crate::channels::MessageOptions {
                silent: false,
                show_typing: false,
                custom: std::collections::HashMap::new(),
            },
            usage: ctx.input.usage,
        };
        if let Err(e) = self
            .dispatcher
            .dispatch(&ctx.input.channel, outbound_msg)
            .await
        {
            tracing::warn!("Reply dispatch failed for channel {}: {}", ctx.input.channel, e);
        }
        OutboundStageAction::Continue
    }
}

/// Side effects stage – memory storage, cron, webhooks, etc.
pub struct SideEffectStage {
    executor: Arc<SideEffectExecutor>,
}

impl SideEffectStage {
    pub fn new(executor: SideEffectExecutor) -> Self {
        Self { executor: Arc::new(executor) }
    }

    /// Create from an already-`Arc`-wrapped executor.
    pub fn from_arc(executor: Arc<SideEffectExecutor>) -> Self {
        Self { executor }
    }
}

#[async_trait]
impl OutboundStage for SideEffectStage {
    fn name(&self) -> &str {
        "side_effects"
    }

    async fn process(&self, ctx: &mut OutboundStageContext) -> OutboundStageAction {
        if !ctx.result.side_effects.is_empty() {
            self.executor.execute_batch(&ctx.result.side_effects).await;
        }
        OutboundStageAction::Continue
    }
}

// ── Stage runner ─────────────────────────────────────────────────────────────

/// Run a slice of outbound stages sequentially.
///
/// Returns the final `OutboundResult` after all stages complete (potentially
/// skipping dispatch), or the partial result if `SkipDispatch` was returned.
pub async fn run_outbound_stages(
    stages: &[Box<dyn OutboundStage>],
    ctx: &mut OutboundStageContext,
) -> OutboundResult {
    for stage in stages {
        match stage.process(ctx).await {
            OutboundStageAction::Continue => continue,
            OutboundStageAction::SkipDispatch => {
                tracing::debug!("Outbound dispatch skipped by stage '{}'", stage.name());
                ctx.skip_dispatch = true;
            }
        }
    }
    ctx.result.clone()
}

// ── Default stage list helper ────────────────────────────────────────────────

/// Build the default list of outbound stages from Arc-wrapped dependencies.
///
/// Used by [`DefaultOutboundPipeline`] which holds `Arc<T>` references.
/// Stages: Canvas → SSE → ReplyPrefix → Dispatch → SideEffects
pub fn default_outbound_stages_from_arcs(
    sse: Option<Arc<SseStreamer>>,
    reply_prefix_engine: Option<ReplyPrefixEngine>,
    dispatcher: Arc<ReplyDispatcher>,
    side_effects: Arc<SideEffectExecutor>,
) -> Vec<Box<dyn OutboundStage>> {
    let mut stages: Vec<Box<dyn OutboundStage>> = Vec::new();

    stages.push(Box::new(CanvasStage));

    if let Some(sse) = sse {
        stages.push(Box::new(SseStage::from_arc(sse)));
    }
    if let Some(engine) = reply_prefix_engine {
        stages.push(Box::new(ReplyPrefixStage::new(engine)));
    }

    stages.push(Box::new(DispatchStage::from_arc(dispatcher)));
    stages.push(Box::new(SideEffectStage::from_arc(side_effects)));
    stages
}

/// Build the default list of outbound stages matching the current pipeline
/// order.
///
/// Stages: Canvas → SSE → ReplyPrefix → Dispatch → SideEffects
pub fn default_outbound_stages(
    sse: Option<SseStreamer>,
    reply_prefix_engine: Option<ReplyPrefixEngine>,
    dispatcher: ReplyDispatcher,
    side_effects: SideEffectExecutor,
) -> Vec<Box<dyn OutboundStage>> {
    let mut stages: Vec<Box<dyn OutboundStage>> = Vec::new();

    stages.push(Box::new(CanvasStage));

    if let Some(sse) = sse {
        stages.push(Box::new(SseStage::new(sse)));
    }
    if let Some(engine) = reply_prefix_engine {
        stages.push(Box::new(ReplyPrefixStage::new(engine)));
    }

    stages.push(Box::new(DispatchStage::new(dispatcher)));
    stages.push(Box::new(SideEffectStage::new(side_effects)));
    stages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::reply_prefix::ReplyPrefixTemplate;
    use crate::channels::ConversationId;
    use crate::outbound::ReplyDispatchConfig;

    // ── Mock Channel for DispatchStage tests ─────────────────────────────

    struct MockChannel {
        messages: Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl crate::channels::Channel for MockChannel {
        fn name(&self) -> &str {
            "mock"
        }
        fn capabilities(&self) -> crate::channels::ChannelCapabilities {
            crate::channels::ChannelCapabilities::default()
        }
        async fn start(&self) -> crate::Result<()> {
            Ok(())
        }
        async fn stop(&self) -> crate::Result<()> {
            Ok(())
        }
        async fn send(
            &self,
            msg: crate::channels::OutgoingMessage,
        ) -> crate::Result<crate::core::Id> {
            self.messages.lock().await.push(msg.content);
            Ok(crate::core::Id::new())
        }
        async fn send_typing(&self, _conversation_id: &ConversationId) -> crate::Result<()> {
            Ok(())
        }
        async fn edit_message(
            &self,
            _message_id: crate::core::Id,
            _new_content: String,
        ) -> crate::Result<()> {
            Ok(())
        }
        async fn delete_message(&self, _message_id: crate::core::Id) -> crate::Result<()> {
            Ok(())
        }
        async fn health_check(&self) -> crate::Result<bool> {
            Ok(true)
        }
    }

    // ── Stage test helpers ───────────────────────────────────────────────

    /// A stage that always continues.
    struct PassStage;

    #[async_trait]
    impl OutboundStage for PassStage {
        fn name(&self) -> &str {
            "pass"
        }
        async fn process(&self, _ctx: &mut OutboundStageContext) -> OutboundStageAction {
            OutboundStageAction::Continue
        }
    }

    /// A stage that signals SkipDispatch.
    struct SkipStage;

    #[async_trait]
    impl OutboundStage for SkipStage {
        fn name(&self) -> &str {
            "skip"
        }
        async fn process(&self, ctx: &mut OutboundStageContext) -> OutboundStageAction {
            ctx.skip_dispatch = true;
            OutboundStageAction::SkipDispatch
        }
    }

    fn dummy_context() -> OutboundStageContext {
        OutboundStageContext::new(OutboundContext {
            session_id: "sess-1".into(),
            conversation_id: None,
            channel: "test".into(),
            agent_id: "agent-1".into(),
            raw_output: "hello world".into(),
            tool_calls: vec![],
            usage: None,
            side_effects: vec![],
            model_name: None,
            model_provider: None,
            reasoning_content: None,
        })
    }

    #[tokio::test]
    async fn test_all_continue() {
        let stages: Vec<Box<dyn OutboundStage>> = vec![Box::new(PassStage), Box::new(PassStage)];
        let mut ctx = dummy_context();
        let result = run_outbound_stages(&stages, &mut ctx).await;
        assert_eq!(result.text, "hello world");
        assert!(!ctx.skip_dispatch);
    }

    #[tokio::test]
    async fn test_skip_dispatch() {
        let stages: Vec<Box<dyn OutboundStage>> = vec![Box::new(PassStage), Box::new(SkipStage)];
        let mut ctx = dummy_context();
        let result = run_outbound_stages(&stages, &mut ctx).await;
        assert_eq!(result.text, "hello world");
        assert!(ctx.skip_dispatch);
    }

    // ── CanvasStage tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_canvas_stage_valid_json() {
        let stage = CanvasStage;
        let mut ctx = dummy_context();
        ctx.result.text = r#"{"type":"text","id":"t1","content":"hello"}"#.into();
        stage.process(&mut ctx).await;
        assert!(ctx.result.canvas_update.is_some());
    }

    #[tokio::test]
    async fn test_canvas_stage_plain_text() {
        let stage = CanvasStage;
        let mut ctx = dummy_context();
        ctx.result.text = "hello world".into();
        stage.process(&mut ctx).await;
        assert!(ctx.result.canvas_update.is_none());
    }

    // ── SseStage tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_sse_stage_tool_events() {
        let streamer = Arc::new(SseStreamer::new());
        let mut rx = streamer.subscribe("sess-1").await;

        let mut ctx = dummy_context();
        ctx.input.tool_calls = vec![crate::providers::ToolCall {
            id: "call-1".into(),
            call_type: "function".into(),
            function: crate::providers::FunctionCall {
                name: "search".into(),
                arguments: "{}".to_string(),
            },
            index: None,
            result: None,
        }];

        let stage = SseStage::from_arc(streamer);
        stage.process(&mut ctx).await;

        // Should have received: ToolStart, ToolEnd, Done in order
        let evt1 = rx.recv().await.unwrap();
        assert!(matches!(evt1, SseEvent::ToolStart { .. }));

        let evt2 = rx.recv().await.unwrap();
        assert!(matches!(evt2, SseEvent::ToolEnd { .. }));

        let evt3 = rx.recv().await.unwrap();
        assert!(matches!(evt3, SseEvent::Done));

        // Also verify the result records 3 events
        assert_eq!(ctx.result.sse_events.len(), 3);
    }

    // ── DispatchStage tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_dispatch_stage_sends_message() {
        let messages = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let channel = Arc::new(MockChannel { messages: messages.clone() });

        let dispatcher = Arc::new(ReplyDispatcher::new(ReplyDispatchConfig::default()));
        dispatcher.register_channel("test", channel).await;

        let stage = DispatchStage::from_arc(dispatcher);
        let mut ctx = dummy_context();
        ctx.result.text = "hello from dispatch".into();

        stage.process(&mut ctx).await;

        let sent = messages.lock().await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0], "hello from dispatch");
    }

    // ── ReplyPrefixStage tests ───────────────────────────────────────────

    #[tokio::test]
    async fn test_reply_prefix_stage_applies_prefix() {
        let engine = ReplyPrefixEngine::new();
        engine
            .set_templates(vec![ReplyPrefixTemplate::new("[model: {{model_name}}] ")])
            .await;
        let stage = ReplyPrefixStage::new(engine);

        let mut ctx = dummy_context();
        ctx.input.model_name = Some("claude-sonnet-4-6".into());
        ctx.result.text = "hello".into();

        stage.process(&mut ctx).await;

        assert!(ctx.result.text.starts_with("[model: claude-sonnet-4-6]"));
        assert!(ctx.result.text.contains("hello"));
    }
}
