//! Pluggable inbound pipeline stages.
//!
//! Defines the [`InboundStage`] trait and a [`InboundContext`] shared between
//! stages, plus stage wrappers for each existing pipeline step.
//!
//! # Pipeline split
//!
//! The debounce stage is special: it decides whether a message should be
//! absorbed into a pending batch or released immediately. Therefore the
//! pipeline is split into two lists:
//!
//! * **Pre-debounce stages** – run once when a message first enters the
//!   pipeline. Currently: identity validation, debounce.
//! * **Post-debounce stages** – run for every message that leaves the debouncer
//!   (either because it bypassed debounce or because a batch was flushed).
//!   Currently: dispatch, media, envelope, queue, router.
//!
//! This avoids the cycle that would occur if a flushed message ran through the
//! debounce stage a second time.
//!
//! # Stage errors
//!
//! Each stage returns [`Result<InboundStageAction, StageError>`]. Terminal
//! actions (`Suppress`, `Debounce`) are represented as successful `Ok(...)`
//! values because they are expected control-flow outcomes, not failures.
//! [`StageError::Fatal`] is reserved for unexpected failures (database errors,
//! channel send failures, etc.).

use std::sync::Arc;

use async_trait::async_trait;

use super::router::AgentRouter;
use super::{
    debounce::InboundDebouncer, dispatch::DispatchResult, media::MediaUnderstandingResult,
    queue::QueueMode, router::RouteResult, AutoReplyDispatch, MediaUnderstandingPipeline,
    QueueModeResolver, RoutedMessage,
};
use crate::channels::envelope::{SessionEnvelopeContext, SessionEnvelopeManager};
use crate::channels::identity::IdentityValidator;
use crate::channels::IncomingMessage;

// ── Errors ───────────────────────────────────────────────────────────────────

/// Errors that can occur while running inbound pipeline stages.
#[derive(Debug, thiserror::Error)]
pub enum StageError {
    /// A stage encountered an unexpected failure. The pipeline should stop
    /// and the error should be logged or propagated.
    #[error("stage '{stage}' failed: {source}")]
    Fatal {
        stage: &'static str,
        #[source]
        source: crate::SyscityError,
    },
}

impl StageError {
    /// Convenience constructor for fatal errors that originate as a plain
    /// message. The message is wrapped in [`SyscityError::Internal`].
    pub fn fatal<S: Into<String>>(stage: &'static str, message: S) -> Self {
        Self::Fatal {
            stage,
            source: crate::SyscityError::Internal(message.into()),
        }
    }

    /// Constructor that preserves an existing [`SyscityError`] without
    /// wrapping it in [`SyscityError::Internal`].
    pub fn from_error(stage: &'static str, source: crate::SyscityError) -> Self {
        Self::Fatal { stage, source }
    }
}

// ── Actions ──────────────────────────────────────────────────────────────────

/// What the pipeline should do after this stage returns successfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundStageAction {
    /// Continue to the next stage normally.
    Continue,
    /// Drop the message entirely (used by dispatch suppress).
    Suppress,
    /// Drop the message and tell the sender why: the agent they named is not
    /// one they may use.
    Deny,
    /// Absorb into the debouncer (message will be re-emitted later).
    Debounce,
}

// ── Context ──────────────────────────────────────────────────────────────────

/// Shared mutable state threaded through all inbound stages.
///
/// The original message is owned by the context. Stages may replace it only
/// when they are producing a derived representation of the same message
/// (e.g. the debounce stage collapses a batch into a single synthetic
/// message). Stages must not mutate message identifiers such as `id`,
/// `user_id`, or `conversation_id`.
#[derive(Debug, Clone)]
pub struct InboundContext {
    /// The incoming message currently being processed.
    pub message: IncomingMessage,
    /// Result of media understanding (populated by [`MediaStage`]).
    pub media_result: Option<MediaUnderstandingResult>,
    /// Result of the dispatch stage.
    pub dispatch_result: Option<DispatchResult>,
    /// Resolved queue mode.
    pub queue_mode: Option<QueueMode>,
    /// Result of agent routing.
    pub route_result: Option<RouteResult>,
    /// Session envelope context (populated by [`EnvelopeStage`]).
    pub envelope_context: Option<SessionEnvelopeContext>,
    /// The agent the router refused to hand this message to, if any.
    ///
    /// Set by [`RouterStage`] when the sender is not an allowed principal for
    /// the agent their message resolved to. The pipeline turns it into a
    /// reply rather than dropping the message silently.
    pub denied_agent: Option<String>,
}

impl InboundContext {
    pub fn new(message: IncomingMessage) -> Self {
        Self {
            message,
            media_result: None,
            dispatch_result: None,
            queue_mode: None,
            route_result: None,
            envelope_context: None,
            denied_agent: None,
        }
    }
}

// ── Trait ────────────────────────────────────────────────────────────────────

/// A single stage in the inbound pipeline.
///
/// Stages are executed in order. Each stage inspects or mutates
/// [`InboundContext`] and returns an [`InboundStageAction`] to indicate
/// whether the pipeline should continue or stop early.
#[async_trait]
pub trait InboundStage: Send + Sync {
    /// Unique name for this stage (used for logging and ordering).
    fn name(&self) -> &'static str;

    /// Process a message. Return [`InboundStageAction::Continue`] to proceed,
    /// [`InboundStageAction::Suppress`] to drop, or
    /// [`InboundStageAction::Debounce`] to absorb into the debouncer.
    ///
    /// Returning an `Err` with [`StageError::Fatal`] aborts the entire
    /// pipeline; callers should log the error and discard the message.
    async fn process(&self, ctx: &mut InboundContext) -> Result<InboundStageAction, StageError>;
}

// ── Built-in stage wrappers ──────────────────────────────────────────────────

/// Identity validation stage.
///
/// By default validation failures are logged and processing continues, but
/// [`IdentityFailMode::Suppress`] can be configured to drop the message
/// instead.
pub struct IdentityStage {
    validator: IdentityValidator,
    fail_mode: IdentityFailMode,
}

/// How the identity stage should behave when validation fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdentityFailMode {
    /// Log a warning and continue processing the message.
    #[default]
    Warn,
    /// Drop the message.
    Suppress,
}

impl IdentityStage {
    pub fn new(validator: IdentityValidator) -> Self {
        Self {
            validator,
            fail_mode: IdentityFailMode::Warn,
        }
    }

    /// Create an identity stage with a custom failure mode.
    pub fn new_with_mode(validator: IdentityValidator, fail_mode: IdentityFailMode) -> Self {
        Self { validator, fail_mode }
    }
}

#[async_trait]
impl InboundStage for IdentityStage {
    fn name(&self) -> &'static str {
        "identity"
    }

    async fn process(&self, ctx: &mut InboundContext) -> Result<InboundStageAction, StageError> {
        let extra = &ctx.message.metadata.extra;

        let mut identity = crate::channels::identity::SenderIdentity {
            user_id: ctx.message.user_id.0.clone(),
            display_name: extract_metadata_string(extra, "display_name"),
            username: extract_metadata_string(extra, "username"),
            phone: extract_metadata_string(extra, "phone"),
            email: extract_metadata_string(extra, "email"),
            raw: extract_metadata_string(extra, "raw"),
            platform_data: Some(build_platform_data(extra, &ctx.message.provenance)),
        };

        // Fall back to the provenance channel name as a raw identifier if no
        // explicit raw value was provided.
        if identity.raw.is_none() {
            if let crate::channels::InputProvenance::ExternalUser { channel, .. } =
                &ctx.message.provenance
            {
                identity.raw = Some(format!("{}:{}", channel, ctx.message.user_id.0));
            }
        }

        if let Err(err) = self.validator.validate(&identity) {
            tracing::warn!(
                message_id = %ctx.message.id,
                user_id = %ctx.message.user_id,
                conversation_id = %ctx.message.conversation_id,
                reason = %err,
                "Identity validation failed"
            );
            return match self.fail_mode {
                IdentityFailMode::Warn => Ok(InboundStageAction::Continue),
                IdentityFailMode::Suppress => Ok(InboundStageAction::Suppress),
            };
        }
        Ok(InboundStageAction::Continue)
    }
}

/// Extract a non-empty string from metadata extra fields.
fn extract_metadata_string(
    extra: &std::collections::HashMap<String, serde_json::Value>,
    key: &str,
) -> Option<String> {
    extra
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Build platform_data from metadata extras plus provenance.
fn build_platform_data(
    extra: &std::collections::HashMap<String, serde_json::Value>,
    provenance: &crate::channels::InputProvenance,
) -> serde_json::Value {
    let mut data: serde_json::Map<String, serde_json::Value> = extra.clone().into_iter().collect();
    if let Ok(value) = serde_json::to_value(provenance) {
        data.insert("provenance".to_string(), value);
    }
    serde_json::Value::Object(data)
}

/// Pre-dispatch cheap suppress check.
///
/// Runs before debounce so obvious drops (send policy denies, group chats
/// without mention) are rejected without the cost of buffering, media
/// understanding, or routing.
pub struct PreDispatchStage {
    dispatch: Arc<AutoReplyDispatch>,
}

impl PreDispatchStage {
    pub fn new(dispatch: AutoReplyDispatch) -> Self {
        Self { dispatch: Arc::new(dispatch) }
    }

    /// Create a pre-dispatch stage from an existing `Arc`.
    pub fn from_arc(dispatch: Arc<AutoReplyDispatch>) -> Self {
        Self { dispatch }
    }
}

#[async_trait]
impl InboundStage for PreDispatchStage {
    fn name(&self) -> &'static str {
        "pre_dispatch"
    }

    async fn process(&self, ctx: &mut InboundContext) -> Result<InboundStageAction, StageError> {
        let result = self.dispatch.cheap_check(&ctx.message).await;
        if result.suppress {
            ctx.dispatch_result = Some(result);
            Ok(InboundStageAction::Suppress)
        } else {
            Ok(InboundStageAction::Continue)
        }
    }
}

/// Debounce stage – absorbs messages that should be batched.
///
/// This stage belongs **only** in the pre-debounce pipeline. If it returns
/// [`InboundStageAction::Debounce`], the caller must not run post-debounce
/// stages until the debouncer emits the message later.
pub struct DebounceStage {
    debouncer: Arc<InboundDebouncer>,
}

impl DebounceStage {
    pub fn new(debouncer: Arc<InboundDebouncer>) -> Self {
        Self { debouncer }
    }
}

#[async_trait]
impl InboundStage for DebounceStage {
    fn name(&self) -> &'static str {
        "debounce"
    }

    async fn process(&self, ctx: &mut InboundContext) -> Result<InboundStageAction, StageError> {
        match self.debouncer.enqueue(ctx.message.clone()).await {
            Some(debounced) => {
                ctx.message = debounced;
                Ok(InboundStageAction::Continue)
            }
            None => Ok(InboundStageAction::Debounce),
        }
    }
}

/// Media understanding stage.
pub struct MediaStage {
    pipeline: MediaUnderstandingPipeline,
}

impl MediaStage {
    pub fn new(pipeline: MediaUnderstandingPipeline) -> Self {
        Self { pipeline }
    }
}

#[async_trait]
impl InboundStage for MediaStage {
    fn name(&self) -> &'static str {
        "media"
    }

    async fn process(&self, ctx: &mut InboundContext) -> Result<InboundStageAction, StageError> {
        ctx.media_result = if !ctx.message.attachments.is_empty() {
            Some(self.pipeline.process(&ctx.message).await)
        } else {
            None
        };
        Ok(InboundStageAction::Continue)
    }
}

/// Dispatch stage – applies send policies, plugin bindings, may suppress.
pub struct DispatchStage {
    dispatch: Arc<AutoReplyDispatch>,
}

impl DispatchStage {
    pub fn new(dispatch: AutoReplyDispatch) -> Self {
        Self { dispatch: Arc::new(dispatch) }
    }

    /// Provide an already-created Arc to share ownership.
    pub fn from_arc(dispatch: Arc<AutoReplyDispatch>) -> Self {
        Self { dispatch }
    }
}

#[async_trait]
impl InboundStage for DispatchStage {
    fn name(&self) -> &'static str {
        "dispatch"
    }

    async fn process(&self, ctx: &mut InboundContext) -> Result<InboundStageAction, StageError> {
        let result = self
            .dispatch
            .process(&ctx.message, ctx.media_result.as_ref())
            .await;
        let suppress = result.suppress;
        ctx.dispatch_result = Some(result);
        if suppress {
            Ok(InboundStageAction::Suppress)
        } else {
            Ok(InboundStageAction::Continue)
        }
    }
}

/// Session envelope tracking stage.
pub struct EnvelopeStage {
    envelope_manager: Arc<SessionEnvelopeManager>,
}

impl EnvelopeStage {
    pub fn new(envelope_manager: SessionEnvelopeManager) -> Self {
        Self {
            envelope_manager: Arc::new(envelope_manager),
        }
    }
}

#[async_trait]
impl InboundStage for EnvelopeStage {
    fn name(&self) -> &'static str {
        "envelope"
    }

    async fn process(&self, ctx: &mut InboundContext) -> Result<InboundStageAction, StageError> {
        let envelope = self
            .envelope_manager
            .get_or_create(&ctx.message.conversation_id.0)
            .await;
        ctx.envelope_context = Some(envelope);
        Ok(InboundStageAction::Continue)
    }
}

/// Queue mode resolution stage.
pub struct QueueStage {
    resolver: QueueModeResolver,
}

impl QueueStage {
    pub fn new(resolver: QueueModeResolver) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl InboundStage for QueueStage {
    fn name(&self) -> &'static str {
        "queue"
    }

    async fn process(&self, ctx: &mut InboundContext) -> Result<InboundStageAction, StageError> {
        ctx.queue_mode = Some(self.resolver.resolve(&ctx.message).await);
        Ok(InboundStageAction::Continue)
    }
}

/// Agent routing stage – determines which agent handles the message.
///
/// This stage only populates `ctx.route_result`. The caller is responsible for
/// constructing the final [`RoutedMessage`] and forwarding it to the agent
/// execution layer. Keeping routing and dispatch separate makes the stage
/// easier to test and avoids giving a stage direct access to an outbound
/// channel.
pub struct RouterStage {
    router: Arc<AgentRouter>,
}

impl RouterStage {
    pub fn new(router: AgentRouter) -> Self {
        Self { router: Arc::new(router) }
    }

    /// Create a router stage from an existing `Arc` to share ownership.
    pub fn from_arc(router: Arc<AgentRouter>) -> Self {
        Self { router }
    }
}

#[async_trait]
impl InboundStage for RouterStage {
    fn name(&self) -> &'static str {
        "router"
    }

    async fn process(&self, ctx: &mut InboundContext) -> Result<InboundStageAction, StageError> {
        let workspace_hint = ctx
            .dispatch_result
            .as_ref()
            .and_then(|d| d.workspace_hint.as_deref());
        match self.router.route(&ctx.message, workspace_hint).await {
            crate::inbound::RouteOutcome::Routed(route) => {
                ctx.route_result = Some(route);
                Ok(InboundStageAction::Continue)
            }
            crate::inbound::RouteOutcome::Denied { agent_id } => {
                // A distinct outcome from `Suppress`: the person asked for an
                // agent they may not use, and has to be told so rather than
                // have their message vanish.
                ctx.denied_agent = Some(agent_id);
                Ok(InboundStageAction::Deny)
            }
        }
    }
}

// ── Routed message construction ──────────────────────────────────────────────

/// Build a [`RoutedMessage`] from a fully processed context.
///
/// Returns [`StageError::Fatal`] if the router stage has not populated
/// `ctx.route_result`. Callers must only invoke this after running the
/// post-debounce stages.
pub fn build_routed_message(ctx: &mut InboundContext) -> Result<RoutedMessage, StageError> {
    let route = ctx
        .route_result
        .clone()
        .ok_or_else(|| StageError::fatal("router", "route_result missing; router stage not run"))?;

    Ok(RoutedMessage {
        incoming: ctx.message.clone(),
        agent_id: route.agent_id,
        workspace_id: route.workspace_id,
        queue_mode: ctx.queue_mode.unwrap_or(QueueMode::Normal),
        suppress_delivery: ctx
            .dispatch_result
            .as_ref()
            .map(|d| d.suppress)
            .unwrap_or(false),
        media_results: ctx.media_result.take(),
        envelope_context: ctx.envelope_context.take(),
    })
}

// ── Stage runner ─────────────────────────────────────────────────────────────

/// Run a slice of stages sequentially.
///
/// Returns `Ok(InboundStageAction::Continue)` if all stages returned
/// [`InboundStageAction::Continue`]. Returns the terminal action as
/// `Ok(Suppress)` or `Ok(Debounce)` if a stage stopped the pipeline. Returns
/// [`StageError::Fatal`] if a stage failed.
pub async fn run_inbound_stages(
    stages: &[Box<dyn InboundStage>],
    ctx: &mut InboundContext,
) -> Result<InboundStageAction, StageError> {
    for stage in stages {
        match stage.process(ctx).await? {
            InboundStageAction::Continue => continue,
            action @ (InboundStageAction::Suppress
            | InboundStageAction::Deny
            | InboundStageAction::Debounce) => {
                tracing::debug!(stage = stage.name(), ?action, "Inbound pipeline terminal action");
                return Ok(action);
            }
        }
    }
    Ok(InboundStageAction::Continue)
}

// ── Default stage list helpers ───────────────────────────────────────────────

/// Build the default list of **pre-debounce** inbound stages.
///
/// Stages: Identity (if `validator` is provided, with the given `fail_mode`)
/// → Pre-dispatch cheap suppress check → Debounce
pub fn default_pre_debounce_stages(
    validator: Option<IdentityValidator>,
    fail_mode: IdentityFailMode,
    dispatch: AutoReplyDispatch,
    debouncer: Arc<InboundDebouncer>,
) -> Vec<Box<dyn InboundStage>> {
    let mut stages: Vec<Box<dyn InboundStage>> = Vec::new();
    if let Some(validator) = validator {
        stages.push(Box::new(IdentityStage::new_with_mode(validator, fail_mode)));
    }
    stages.push(Box::new(PreDispatchStage::new(dispatch)));
    stages.push(Box::new(DebounceStage::new(debouncer)));
    stages
}

/// Build the default list of **post-debounce** inbound stages.
///
/// Stages: Dispatch → Media → Envelope (if `envelope_manager` is provided)
/// → Queue → Router
pub fn default_post_debounce_stages(
    media_pipeline: MediaUnderstandingPipeline,
    dispatch: AutoReplyDispatch,
    envelope_manager: Option<SessionEnvelopeManager>,
    queue_resolver: QueueModeResolver,
    router: Arc<AgentRouter>,
) -> Vec<Box<dyn InboundStage>> {
    let mut stages: Vec<Box<dyn InboundStage>> = vec![
        Box::new(DispatchStage::new(dispatch)),
        Box::new(MediaStage::new(media_pipeline)),
    ];
    if let Some(envelope_manager) = envelope_manager {
        stages.push(Box::new(EnvelopeStage::new(envelope_manager)));
    }
    stages.push(Box::new(QueueStage::new(queue_resolver)));
    stages.push(Box::new(RouterStage::from_arc(router)));
    stages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::IncomingMessage;
    use crate::channels::MessageMetadata;
    use crate::inbound::dispatch::AutoReplyDispatchConfig;

    /// A stage that always suppresses.
    struct SuppressStage;

    #[async_trait]
    impl InboundStage for SuppressStage {
        fn name(&self) -> &'static str {
            "suppress"
        }
        async fn process(
            &self,
            _ctx: &mut InboundContext,
        ) -> Result<InboundStageAction, StageError> {
            Ok(InboundStageAction::Suppress)
        }
    }

    /// A stage that always continues.
    struct PassStage;

    #[async_trait]
    impl InboundStage for PassStage {
        fn name(&self) -> &'static str {
            "pass"
        }
        async fn process(
            &self,
            _ctx: &mut InboundContext,
        ) -> Result<InboundStageAction, StageError> {
            Ok(InboundStageAction::Continue)
        }
    }

    /// A stage that records how many times it ran.
    struct CountStage {
        counter: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl InboundStage for CountStage {
        fn name(&self) -> &'static str {
            "count"
        }
        async fn process(
            &self,
            _ctx: &mut InboundContext,
        ) -> Result<InboundStageAction, StageError> {
            self.counter
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(InboundStageAction::Continue)
        }
    }

    fn dummy_message() -> IncomingMessage {
        IncomingMessage {
            id: crate::core::models::Id::new(),
            user_id: crate::channels::UserId::new("user1"),
            conversation_id: crate::channels::ConversationId::new("conv1"),
            content: "hello".into(),
            attachments: vec![],
            metadata: MessageMetadata::new(),
            provenance: crate::channels::InputProvenance::ExternalUser {
                channel: "test".into(),
                is_direct: true,
            },
            mention: crate::channels::MentionState::DirectMessage,
        }
    }

    #[tokio::test]
    async fn test_all_continue() {
        let stages: Vec<Box<dyn InboundStage>> = vec![Box::new(PassStage), Box::new(PassStage)];
        let mut ctx = InboundContext::new(dummy_message());
        let result = run_inbound_stages(&stages, &mut ctx).await;
        assert_eq!(result.unwrap(), InboundStageAction::Continue);
    }

    #[tokio::test]
    async fn test_suppress_early_exit() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stages: Vec<Box<dyn InboundStage>> = vec![
            Box::new(CountStage { counter: counter.clone() }),
            Box::new(SuppressStage),
            Box::new(CountStage { counter: counter.clone() }),
        ];
        let mut ctx = InboundContext::new(dummy_message());
        let result = run_inbound_stages(&stages, &mut ctx).await;
        assert_eq!(result.unwrap(), InboundStageAction::Suppress);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_debounce_early_exit() {
        struct DebounceStage_;
        #[async_trait]
        impl InboundStage for DebounceStage_ {
            fn name(&self) -> &'static str {
                "db"
            }
            async fn process(
                &self,
                _: &mut InboundContext,
            ) -> Result<InboundStageAction, StageError> {
                Ok(InboundStageAction::Debounce)
            }
        }

        let stages: Vec<Box<dyn InboundStage>> = vec![
            Box::new(PassStage),
            Box::new(DebounceStage_),
            Box::new(PassStage),
        ];
        let mut ctx = InboundContext::new(dummy_message());
        let result = run_inbound_stages(&stages, &mut ctx).await;
        assert_eq!(result.unwrap(), InboundStageAction::Debounce);
    }

    #[tokio::test]
    async fn test_fatal_stage_error() {
        struct ErrorStage;
        #[async_trait]
        impl InboundStage for ErrorStage {
            fn name(&self) -> &'static str {
                "error"
            }
            async fn process(
                &self,
                _: &mut InboundContext,
            ) -> Result<InboundStageAction, StageError> {
                Err(StageError::fatal("error", "simulated failure"))
            }
        }

        let stages: Vec<Box<dyn InboundStage>> = vec![Box::new(PassStage), Box::new(ErrorStage)];
        let mut ctx = InboundContext::new(dummy_message());
        let result = run_inbound_stages(&stages, &mut ctx).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("error"));
        assert!(err.contains("simulated failure"));
    }

    #[tokio::test]
    async fn test_stage_error_preserves_source_type() {
        let storage_err = crate::SyscityError::Storage {
            context: "test".to_string(),
            details: "connection lost".to_string(),
        };
        let err = StageError::from_error("db", storage_err);
        let err_string = err.to_string();
        assert!(err_string.contains("db"));
        assert!(err_string.contains("Storage error"));
        assert!(err_string.contains("connection lost"));
    }

    #[tokio::test]
    async fn test_dispatch_stage_suppresses() {
        let dispatch = AutoReplyDispatch::new(AutoReplyDispatchConfig {
            suppress_unless_mentioned_in_groups: true,
            ..Default::default()
        });
        let stage = DispatchStage::new(dispatch);
        let mut msg = dummy_message();
        msg.provenance = crate::channels::InputProvenance::ExternalUser {
            channel: "test".into(),
            is_direct: false,
        };
        msg.mention = crate::channels::MentionState::NotMentioned;

        let mut ctx = InboundContext::new(msg);
        let action = stage.process(&mut ctx).await.unwrap();
        assert_eq!(action, InboundStageAction::Suppress);
        assert!(ctx.dispatch_result.is_some());
    }

    #[tokio::test]
    async fn test_queue_stage_sets_mode() {
        let stage = QueueStage::new(QueueModeResolver::new());
        let mut ctx = InboundContext::new(IncomingMessage::new("u1", "s1", "!stop"));
        let action = stage.process(&mut ctx).await.unwrap();
        assert_eq!(action, InboundStageAction::Continue);
        assert_eq!(ctx.queue_mode, Some(QueueMode::Interrupt));
    }

    #[tokio::test]
    async fn test_build_routed_message_requires_route() {
        let mut ctx = InboundContext::new(dummy_message());
        let result = build_routed_message(&mut ctx);
        assert!(result.is_err(), "build_routed_message must error without route_result");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("router"));
        assert!(err.contains("route_result missing"));
    }

    #[test]
    fn test_extract_metadata_string_skips_empty() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("username".to_string(), serde_json::json!("alice"));
        extra.insert("display_name".to_string(), serde_json::json!(""));
        extra.insert("phone".to_string(), serde_json::json!(12345));

        assert_eq!(extract_metadata_string(&extra, "username"), Some("alice".to_string()));
        assert_eq!(extract_metadata_string(&extra, "display_name"), None);
        assert_eq!(extract_metadata_string(&extra, "phone"), None);
        assert_eq!(extract_metadata_string(&extra, "missing"), None);
    }

    #[test]
    fn test_build_platform_data_includes_provenance() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("username".to_string(), serde_json::json!("alice"));
        let provenance = crate::channels::InputProvenance::ExternalUser {
            channel: "telegram".to_string(),
            is_direct: true,
        };

        let data = build_platform_data(&extra, &provenance);
        let obj = data.as_object().expect("platform_data should be an object");
        assert_eq!(obj.get("username"), Some(&serde_json::json!("alice")));
        assert!(obj.contains_key("provenance"));
    }

    #[tokio::test]
    async fn test_identity_stage_warn_mode_continues() {
        let validator = crate::channels::identity::IdentityValidator::with_config(
            crate::channels::identity::IdentityValidatorConfig {
                user_id_allowed_chars: crate::channels::identity::AllowedCharSet::AlphanumericOnly,
                ..Default::default()
            },
        );
        let stage = IdentityStage::new_with_mode(validator, IdentityFailMode::Warn);
        let mut ctx = InboundContext::new(IncomingMessage::new("user-1", "s1", "hello"));
        let action = stage.process(&mut ctx).await.unwrap();
        assert_eq!(action, InboundStageAction::Continue);
    }

    #[tokio::test]
    async fn test_identity_stage_suppress_mode_drops() {
        let validator = crate::channels::identity::IdentityValidator::with_config(
            crate::channels::identity::IdentityValidatorConfig {
                user_id_allowed_chars: crate::channels::identity::AllowedCharSet::AlphanumericOnly,
                ..Default::default()
            },
        );
        let stage = IdentityStage::new_with_mode(validator, IdentityFailMode::Suppress);
        let mut ctx = InboundContext::new(IncomingMessage::new("user-1", "s1", "hello"));
        let action = stage.process(&mut ctx).await.unwrap();
        assert_eq!(action, InboundStageAction::Suppress);
    }

    #[tokio::test]
    async fn test_pre_dispatch_stage_suppresses_group_message() {
        let dispatch = AutoReplyDispatch::new(AutoReplyDispatchConfig {
            suppress_unless_mentioned_in_groups: true,
            ..Default::default()
        });
        let stage = PreDispatchStage::new(dispatch);
        let mut msg = dummy_message();
        msg.provenance = crate::channels::InputProvenance::ExternalUser {
            channel: "telegram".to_string(),
            is_direct: false,
        };
        msg.mention = crate::channels::MentionState::NotMentioned;

        let mut ctx = InboundContext::new(msg);
        let action = stage.process(&mut ctx).await.unwrap();
        assert_eq!(action, InboundStageAction::Suppress);
        assert!(ctx.dispatch_result.as_ref().unwrap().suppress);
    }

    #[tokio::test]
    async fn test_pre_dispatch_stage_allows_direct_message() {
        let dispatch = AutoReplyDispatch::new(AutoReplyDispatchConfig::default());
        let stage = PreDispatchStage::new(dispatch);
        let mut ctx = InboundContext::new(dummy_message());
        let action = stage.process(&mut ctx).await.unwrap();
        assert_eq!(action, InboundStageAction::Continue);
        assert!(ctx.dispatch_result.is_none());
    }
}
