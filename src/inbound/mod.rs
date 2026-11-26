//! Inbound Pipeline for Syscity
//!
//! The inbound pipeline is the entry point for all user messages.
//! It replaces the direct "Channel -> Gateway -> Agent" path with a layered
//! processing pipeline:
//!
//! ```text
//! Channel Extension -> Identity (optional) -> Pre-dispatch cheap suppress check
//! -> Debounce -> Dispatch -> Media Understanding -> Envelope (optional)
//! -> Queue Mode Resolve -> Agent Router -> Agent
//! ```
// INVARIANTS-NONE: message intake plumbing; access decisions are delegated to gateway::check_incoming_access and pairing/security stores.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::warn;

use crate::channels::envelope::SessionEnvelopeManager;
use crate::channels::identity::IdentityValidator;
use crate::channels::IncomingMessage;
use crate::inbound::stage::{
    build_routed_message, default_post_debounce_stages, default_pre_debounce_stages,
    run_inbound_stages, IdentityFailMode, InboundContext, InboundStageAction, StageError,
};

pub mod debounce;
pub mod dispatch;
pub mod media;
pub mod pipeline_tests;
pub mod queue;
pub mod router;
pub mod stage;

pub use debounce::{InboundDebouncer, InboundDebouncerConfig};
pub use dispatch::{AutoReplyDispatch, AutoReplyDispatchConfig, DispatchResult};
pub use media::{MediaAttachmentCache, MediaUnderstandingPipeline, MediaUnderstandingResult};
pub use queue::{QueueMode, QueueModeResolver};
pub use router::{
    AgentRouter, AgentRouterConfig, BindingStore, InMemoryBindingStore, RouteOutcome, RouteResult,
    SqliteBindingStore,
};

/// A message that has been processed by the inbound pipeline and is ready
/// for routing to an agent.
#[derive(Debug, Clone)]
pub struct RoutedMessage {
    /// The original incoming message
    pub incoming: IncomingMessage,
    /// Target agent ID (resolved by router)
    pub agent_id: String,
    /// Target workspace ID (if multi-workspace)
    pub workspace_id: Option<String>,
    /// Queue mode for this message
    pub queue_mode: QueueMode,
    /// Whether delivery should be suppressed (silent mode)
    pub suppress_delivery: bool,
    /// Media understanding results (if any attachments were processed)
    pub media_results: Option<MediaUnderstandingResult>,
    /// Session envelope context captured during pipeline processing.
    pub envelope_context: Option<crate::channels::envelope::SessionEnvelopeContext>,
}

/// The inbound pipeline trait.
///
/// Implementations process raw incoming messages through the full pipeline
/// (debounce, media, dispatch, queue, router) and produce `RoutedMessage`s.
#[async_trait::async_trait]
pub trait InboundPipeline: Send + Sync {
    /// Process a single incoming message.
    ///
    /// Returns `None` if the message was absorbed by the pipeline
    /// (e.g., debounced, filtered, or queued for later).
    async fn process(&self, message: IncomingMessage) -> Option<RoutedMessage>;

    /// Process a message, keeping the reason it did not produce a turn.
    ///
    /// `process` collapses every non-routing result into `None`, which is fine
    /// for a caller that only wants the routed message. The dispatch worker
    /// needs the difference: a message refused for its sender's principal has
    /// to be answered, while a debounced one must stay quiet.
    ///
    /// The default keeps any implementation that cannot distinguish the cases
    /// behaving exactly as `process` did.
    async fn process_detailed(&self, message: IncomingMessage) -> InboundProcessOutcome {
        match self.process(message).await {
            Some(routed) => InboundProcessOutcome::Routed(Box::new(routed)),
            None => InboundProcessOutcome::Absorbed,
        }
    }

    /// Flush all pending messages for a given key.
    ///
    /// Used at shutdown or when an explicit flush is needed.
    async fn flush(&self, key: &str) -> Vec<RoutedMessage>;
}

/// Detailed outcome of processing a single inbound message.
///
/// Unlike [`InboundPipeline::process`], which collapses all non-routed results
/// into `None`, this enum distinguishes between absorption (debounce),
/// explicit suppression, routing failures, and successful routing.
#[derive(Debug, Clone)]
pub enum InboundProcessOutcome {
    /// The message was routed to an agent.
    Routed(Box<RoutedMessage>),
    /// The message was absorbed into the debouncer and will be processed later.
    Absorbed,
    /// The message was explicitly suppressed by a stage (e.g. dispatch policy).
    Suppressed {
        /// Human-readable reason for suppression, if available.
        reason: Option<String>,
    },
    /// A stage failed unexpectedly.
    Failed {
        /// Name of the stage that failed.
        stage: &'static str,
        /// String representation of the error.
        error: String,
    },
    /// The router refused the agent this message resolved to: the sender is
    /// not an allowed principal for it.
    ///
    /// Distinct from [`Self::Suppressed`] because the sender must be told; a
    /// silent drop would look like the bot being broken.
    Denied {
        /// The agent the message was refused.
        agent_id: String,
        /// The incoming message, so the caller can reply in place.
        message: Box<IncomingMessage>,
    },
}

impl InboundProcessOutcome {
    /// Convert the outcome into the legacy `Option<RoutedMessage>`
    /// representation.
    ///
    /// Returns `Some(message)` only for [`Self::Routed`]. All other variants
    /// return `None`.
    pub fn into_option(self) -> Option<RoutedMessage> {
        match self {
            Self::Routed(msg) => Some(*msg),
            _ => None,
        }
    }
}

/// Build the outcome for a message the router refused.
///
/// The stage records which agent it refused; the incoming message travels with
/// the outcome so the caller can answer the sender in the same conversation.
fn denied_outcome(ctx: InboundContext) -> InboundProcessOutcome {
    let InboundContext { message, denied_agent, .. } = ctx;
    InboundProcessOutcome::Denied {
        agent_id: denied_agent.unwrap_or_default(),
        message: Box::new(message),
    }
}

/// A message the router refused, surfaced from the debounced path.
///
/// A message that goes through the debouncer is flushed by the pipeline's own
/// background loop, so there is no caller left holding an
/// [`InboundProcessOutcome`] to inspect. The pipeline hands the refusal to the
/// sink instead, and the gateway — which is the only layer that can reach a
/// channel — answers the sender.
#[derive(Debug, Clone)]
pub struct DeniedMessage {
    /// The agent the message resolved to and the sender may not use.
    pub agent_id: String,
    /// The message, so the reply can go back to the same conversation.
    pub message: IncomingMessage,
}

/// Default inbound pipeline implementation.
///
/// Wires all stages together through the pluggable [`InboundStage`] runner:
/// identity (optional) → pre-dispatch cheap suppress check → debounce → media
/// → dispatch → envelope (optional) → queue → router.
pub struct DefaultInboundPipeline {
    debouncer: Arc<InboundDebouncer>,
    media_pipeline: MediaUnderstandingPipeline,
    dispatch: AutoReplyDispatch,
    queue_resolver: QueueModeResolver,
    router: Arc<AgentRouter>,
    /// Sender to forward routed messages to the agent execution layer.
    routed_tx: mpsc::Sender<RoutedMessage>,
    /// Receiver for debounced message batches from the debouncer.
    flush_rx: tokio::sync::Mutex<mpsc::Receiver<Vec<crate::inbound::debounce::DebouncedItem>>>,
    /// Optional identity validator for pre-debounce checks.
    identity_validator: Option<IdentityValidator>,
    /// How to handle identity validation failures.
    identity_fail_mode: IdentityFailMode,
    /// Optional session envelope manager for tracking session context.
    envelope_manager: Option<SessionEnvelopeManager>,
    /// Cached pre-debounce stage list to avoid rebuilding it per message.
    pre_stages: Vec<Box<dyn crate::inbound::stage::InboundStage>>,
    /// Cached post-debounce stage list to avoid rebuilding it per message.
    post_stages: Vec<Box<dyn crate::inbound::stage::InboundStage>>,
    /// Whether the background flush loop has already been started.
    started: AtomicBool,
    /// Whether to start the background flush loop automatically when the
    /// pipeline is wrapped in an [`Arc`] and [`DefaultInboundPipeline::start`]
    /// (or [`DefaultInboundPipeline::start_if_configured`]) is called.
    auto_start: bool,
    /// Where refusals from the debounced path go, so the gateway can answer
    /// the sender. `None` when no sink is attached (tests, embedders).
    denied_tx: Option<mpsc::Sender<DeniedMessage>>,
}

impl DefaultInboundPipeline {
    pub fn new(
        debouncer: Arc<InboundDebouncer>,
        media_pipeline: MediaUnderstandingPipeline,
        dispatch: AutoReplyDispatch,
        queue_resolver: QueueModeResolver,
        router: Arc<AgentRouter>,
        routed_tx: mpsc::Sender<RoutedMessage>,
        flush_rx: mpsc::Receiver<Vec<crate::inbound::debounce::DebouncedItem>>,
    ) -> Self {
        let pre_stages = default_pre_debounce_stages(
            None,
            IdentityFailMode::Warn,
            dispatch.clone(),
            debouncer.clone(),
        );
        let post_stages = default_post_debounce_stages(
            media_pipeline.clone(),
            dispatch.clone(),
            None,
            queue_resolver.clone(),
            router.clone(),
        );
        Self {
            debouncer,
            media_pipeline,
            dispatch,
            queue_resolver,
            router,
            routed_tx,
            flush_rx: tokio::sync::Mutex::new(flush_rx),
            identity_validator: None,
            identity_fail_mode: IdentityFailMode::Warn,
            envelope_manager: None,
            pre_stages,
            post_stages,
            started: AtomicBool::new(false),
            auto_start: false,
            denied_tx: None,
        }
    }

    /// Attach a sink for refusals that surface from the debounced path.
    ///
    /// Without it, a refusal on a debounced message has nobody to tell: the
    /// outcome of the flush belongs to the pipeline's own background loop, not
    /// to a caller. The gateway attaches a sink and answers the sender on the
    /// channel the message came from.
    pub fn with_denied_sink(mut self, tx: mpsc::Sender<DeniedMessage>) -> Self {
        self.denied_tx = Some(tx);
        self
    }

    /// Configure whether the pipeline should start its background flush loop
    /// automatically.
    ///
    /// When `auto_start` is `true`, callers can use
    /// [`DefaultInboundPipeline::start_if_configured`] to start the loop
    /// without an extra conditional check. The default is `false`, preserving
    /// the existing explicit-start behavior.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Attach an identity validator for pre-debounce checks.
    pub fn with_identity_validator(mut self, validator: IdentityValidator) -> Self {
        self.identity_validator = Some(validator.clone());
        self.pre_stages = default_pre_debounce_stages(
            Some(validator),
            self.identity_fail_mode,
            self.dispatch.clone(),
            self.debouncer.clone(),
        );
        self
    }

    /// Configure how identity validation failures are handled.
    ///
    /// The default is [`IdentityFailMode::Warn`], which logs a warning and
    /// continues processing. [`IdentityFailMode::Suppress`] drops messages that
    /// fail validation.
    ///
    /// This may be called before or after [`Self::with_identity_validator`];
    /// the pre-debounce stage list is rebuilt when either method is called.
    pub fn with_identity_fail_mode(mut self, fail_mode: IdentityFailMode) -> Self {
        self.identity_fail_mode = fail_mode;
        // If a validator is already attached, rebuild the stage list with the
        // new fail mode.
        if let Some(validator) = self.identity_validator.clone() {
            self.pre_stages = default_pre_debounce_stages(
                Some(validator),
                fail_mode,
                self.dispatch.clone(),
                self.debouncer.clone(),
            );
        }
        self
    }

    /// Attach a session envelope manager for tracking session context.
    pub fn with_envelope_manager(mut self, manager: SessionEnvelopeManager) -> Self {
        self.envelope_manager = Some(manager.clone());
        self.post_stages = default_post_debounce_stages(
            self.media_pipeline.clone(),
            self.dispatch.clone(),
            Some(manager),
            self.queue_resolver.clone(),
            self.router.clone(),
        );
        self
    }

    /// Start a background task that consumes debounced messages and runs
    /// them through the rest of the pipeline.
    ///
    /// This is a no-op if the pipeline has already been started.
    pub fn start(self: Arc<Self>) {
        if self
            .started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            tracing::debug!("DefaultInboundPipeline already started; ignoring duplicate start()");
            return;
        }
        let pipeline = self.clone();
        tokio::spawn(async move {
            pipeline.run_loop().await;
        });
    }

    /// Start the background loop only if [`Self::auto_start`] is enabled.
    ///
    /// This is a convenience for callers that configure the pipeline with
    /// [`Self::with_auto_start`]. It is a no-op when `auto_start` is `false`.
    pub fn start_if_configured(self: Arc<Self>) {
        if self.auto_start {
            self.start();
        }
    }

    /// Process a message and return a detailed outcome.
    ///
    /// This is the richer counterpart to [`InboundPipeline::process`]. Callers
    /// that only need the routed message can use `process()`; callers that need
    /// to distinguish debounce, suppress, and failure should use this method.
    pub async fn process_detailed(&self, message: IncomingMessage) -> InboundProcessOutcome {
        if !self.started.load(Ordering::Relaxed) {
            tracing::warn!(
                "DefaultInboundPipeline::process() called before start(); debounced messages will \
                 not be flushed by the background loop"
            );
        }

        let mut ctx = InboundContext::new(message);

        match run_inbound_stages(&self.pre_stages, &mut ctx).await {
            Ok(InboundStageAction::Continue) => {}
            Ok(InboundStageAction::Suppress) => {
                let reason = ctx
                    .dispatch_result
                    .as_ref()
                    .and_then(|d| d.suppress_reason.clone());
                return InboundProcessOutcome::Suppressed { reason };
            }
            Ok(InboundStageAction::Deny) => {
                return denied_outcome(ctx);
            }
            Ok(InboundStageAction::Debounce) => {
                return InboundProcessOutcome::Absorbed;
            }
            Err(StageError::Fatal { stage, source }) => {
                warn!(stage, error = %source, "Pre-debounce stage failed");
                return InboundProcessOutcome::Failed {
                    stage,
                    error: source.to_string(),
                };
            }
        }

        match run_inbound_stages(&self.post_stages, &mut ctx).await {
            Ok(InboundStageAction::Continue) => match build_routed_message(&mut ctx) {
                Ok(routed) => {
                    if let Err(e) = self.routed_tx.send(routed.clone()).await {
                        warn!(error = %e, "Failed to forward routed message to agent execution layer");
                    }
                    InboundProcessOutcome::Routed(Box::new(routed))
                }
                Err(e) => {
                    warn!(error = %e, "Post-debounce routing failed");
                    InboundProcessOutcome::Failed {
                        stage: "router",
                        error: e.to_string(),
                    }
                }
            },
            Ok(InboundStageAction::Suppress) => {
                let reason = ctx
                    .dispatch_result
                    .as_ref()
                    .and_then(|d| d.suppress_reason.clone());
                InboundProcessOutcome::Suppressed { reason }
            }
            Ok(InboundStageAction::Deny) => denied_outcome(ctx),
            Ok(InboundStageAction::Debounce) => {
                tracing::debug!(
                    "Post-debounce Debounce action is unexpected; treating as Absorbed"
                );
                InboundProcessOutcome::Absorbed
            }
            Err(StageError::Fatal { stage, source }) => {
                warn!(stage, error = %source, "Post-debounce stage failed");
                InboundProcessOutcome::Failed {
                    stage,
                    error: source.to_string(),
                }
            }
        }
    }

    /// Route a single message through the post-debounce stages and forward it
    /// to the agent execution layer.
    ///
    /// This helper is shared between the background flush loop and explicit
    /// flushes so both paths use exactly the same routing logic.
    ///
    /// A refusal found here is reported to the denial sink rather than dropped:
    /// on this path there is no caller holding an outcome, so the sink is the
    /// only way the sender learns why their message went unanswered.
    async fn route_one(&self, message: IncomingMessage) -> Option<RoutedMessage> {
        let mut ctx = InboundContext::new(message);
        match run_inbound_stages(&self.post_stages, &mut ctx).await {
            Ok(InboundStageAction::Continue) => match build_routed_message(&mut ctx) {
                Ok(routed) => {
                    if let Err(e) = self.routed_tx.send(routed.clone()).await {
                        warn!(error = %e, "Failed to forward routed message to agent execution layer (route_one)");
                    }
                    Some(routed)
                }
                Err(e) => {
                    warn!(error = %e, "Routing failed");
                    None
                }
            },
            Ok(InboundStageAction::Deny) => {
                if let InboundProcessOutcome::Denied { agent_id, message } = denied_outcome(ctx) {
                    self.report_denied(agent_id, *message).await;
                }
                None
            }
            Ok(action) => {
                tracing::debug!(?action, "Routing terminal action");
                None
            }
            Err(StageError::Fatal { stage, source }) => {
                warn!(stage, error = %source, "Routing stage failed");
                None
            }
        }
    }

    /// Hand a refusal to the denial sink, if one is attached.
    async fn report_denied(&self, agent_id: String, message: IncomingMessage) {
        let Some(tx) = self.denied_tx.as_ref() else {
            warn!(
                agent = %agent_id,
                user = %message.user_id.0,
                "Refused agent with no denial sink attached; the sender was not told"
            );
            return;
        };
        if let Err(e) = tx.send(DeniedMessage { agent_id, message }).await {
            warn!(error = %e, "Failed to hand a refused message to the denial sink");
        }
    }

    async fn run_loop(self: Arc<Self>) {
        let mut rx = self.flush_rx.lock().await;
        while let Some(batch) = rx.recv().await {
            for item in batch {
                self.route_one(item.message).await;
            }
        }
    }
}

#[async_trait::async_trait]
impl InboundPipeline for DefaultInboundPipeline {
    async fn process(&self, message: IncomingMessage) -> Option<RoutedMessage> {
        self.process_detailed(message).await.into_option()
    }

    async fn process_detailed(&self, message: IncomingMessage) -> InboundProcessOutcome {
        Self::process_detailed(self, message).await
    }

    async fn flush(&self, key: &str) -> Vec<RoutedMessage> {
        let messages = self.debouncer.flush_key(key).await;
        let mut routed = Vec::new();
        for msg in messages {
            if let Some(r) = self.route_one(msg).await {
                routed.push(r);
            }
        }
        routed
    }
}
