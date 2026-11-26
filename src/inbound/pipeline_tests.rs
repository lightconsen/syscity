//! Integration tests for `DefaultInboundPipeline`.
//!
//! These tests exercise the default inbound pipeline end-to-end:
//! identity (optional) → debounce → dispatch → media → envelope (optional)
//! → queue → router.

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::mpsc;

    use crate::channels::envelope::SessionEnvelopeManager;
    use crate::channels::identity::IdentityValidator;
    use crate::channels::{Attachment, IncomingMessage, InputProvenance, MentionState};
    use crate::inbound::debounce::{DebouncedItem, InboundDebouncer, InboundDebouncerConfig};
    use crate::inbound::dispatch::{AutoReplyDispatch, AutoReplyDispatchConfig};
    use crate::inbound::media::MediaUnderstandingPipeline;
    use crate::inbound::queue::{QueueMode, QueueModeResolver};
    use crate::inbound::router::{AgentRouter, AgentRouterConfig};
    use crate::inbound::{
        DefaultInboundPipeline, InboundPipeline, InboundProcessOutcome, RoutedMessage,
    };

    struct PipelineHarness {
        pipeline: Arc<DefaultInboundPipeline>,
        routed_rx: tokio::sync::Mutex<mpsc::Receiver<RoutedMessage>>,
        _temp_dirs: Vec<tempfile::TempDir>,
    }

    impl PipelineHarness {
        async fn new(
            debouncer: Arc<InboundDebouncer>,
            flush_rx: mpsc::Receiver<Vec<DebouncedItem>>,
            dispatch: AutoReplyDispatch,
            router: AgentRouter,
            envelope_manager: Option<(SessionEnvelopeManager, Option<tempfile::TempDir>)>,
        ) -> Self {
            let (routed_tx, routed_rx) = mpsc::channel::<RoutedMessage>(64);
            let mut temp_dirs = Vec::new();
            let envelope_manager = envelope_manager.unwrap_or_else(|| {
                let dir = tempfile::tempdir().expect("failed to create temp envelope dir");
                let path = dir.path().to_path_buf();
                temp_dirs.push(dir);
                (SessionEnvelopeManager::new(path), None)
            });
            if let Some(dir) = envelope_manager.1 {
                temp_dirs.push(dir);
            }
            let pipeline = Arc::new(
                DefaultInboundPipeline::new(
                    debouncer,
                    MediaUnderstandingPipeline::new(),
                    dispatch,
                    QueueModeResolver::new(),
                    Arc::new(router),
                    routed_tx,
                    flush_rx,
                )
                .with_identity_validator(IdentityValidator::new())
                .with_envelope_manager(envelope_manager.0),
            );
            pipeline.clone().start();
            Self {
                pipeline,
                routed_rx: tokio::sync::Mutex::new(routed_rx),
                _temp_dirs: temp_dirs,
            }
        }

        async fn process(
            &self,
            msg: IncomingMessage,
        ) -> (Option<RoutedMessage>, Vec<RoutedMessage>) {
            let mut rx = self.routed_rx.lock().await;
            let result = self.pipeline.process(msg).await;
            // Auto-flush tests debounce for 50ms; give the background flush loop a
            // 1s margin so heavily-loaded CI runners (macOS) don't miss the window.
            let channel_messages = drain_with_timeout(&mut rx, Duration::from_millis(1000)).await;
            (result, channel_messages)
        }

        async fn process_detailed(
            &self,
            msg: IncomingMessage,
        ) -> (InboundProcessOutcome, Vec<RoutedMessage>) {
            let mut rx = self.routed_rx.lock().await;
            let result = self.pipeline.process_detailed(msg).await;
            let channel_messages = drain_with_timeout(&mut rx, Duration::from_millis(1000)).await;
            (result, channel_messages)
        }

        async fn flush(&self, key: &str) -> (Vec<RoutedMessage>, Vec<RoutedMessage>) {
            let mut rx = self.routed_rx.lock().await;
            let result = self.pipeline.flush(key).await;
            let channel_messages = drain_with_timeout(&mut rx, Duration::from_millis(1000)).await;
            (result, channel_messages)
        }
    }

    async fn drain_with_timeout(
        rx: &mut mpsc::Receiver<RoutedMessage>,
        timeout: Duration,
    ) -> Vec<RoutedMessage> {
        let mut messages = Vec::new();
        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Some(first)) => messages.push(first),
            _ => return messages,
        }
        while let Ok(m) = rx.try_recv() {
            messages.push(m);
        }
        messages
    }

    fn assert_routed_message_eq(
        left: &Option<RoutedMessage>,
        right: &Option<RoutedMessage>,
        context: &str,
    ) {
        match (left, right) {
            (None, None) => {}
            (Some(l), Some(r)) => {
                assert_eq!(l.agent_id, r.agent_id, "{context}: agent_id mismatch");
                assert_eq!(l.workspace_id, r.workspace_id, "{context}: workspace_id mismatch");
                assert_eq!(l.queue_mode, r.queue_mode, "{context}: queue_mode mismatch");
                assert_eq!(
                    l.suppress_delivery, r.suppress_delivery,
                    "{context}: suppress_delivery mismatch"
                );
                assert_eq!(l.incoming.content, r.incoming.content, "{context}: content mismatch");
                assert_eq!(
                    l.media_results.is_some(),
                    r.media_results.is_some(),
                    "{context}: media_results presence mismatch"
                );
                assert_eq!(
                    l.envelope_context.is_some(),
                    r.envelope_context.is_some(),
                    "{context}: envelope_context presence mismatch"
                );
            }
            (None, Some(_)) => {
                panic!("{context}: left returned None, right returned Some")
            }
            (Some(_), None) => {
                panic!("{context}: left returned Some, right returned None")
            }
        }
    }

    fn plain_message() -> IncomingMessage {
        IncomingMessage::new("user1", "conv1", "hello")
    }

    fn command_message() -> IncomingMessage {
        IncomingMessage::new("user1", "conv1", "/help")
    }

    fn group_unmentioned_message() -> IncomingMessage {
        IncomingMessage::new("user1", "conv1", "hello")
            .with_provenance(InputProvenance::ExternalUser {
                channel: "telegram".into(),
                is_direct: false,
            })
            .with_mention(MentionState::NotMentioned)
    }

    fn mentioned_message() -> IncomingMessage {
        IncomingMessage::new("user1", "conv1", "hello")
            .with_provenance(InputProvenance::ExternalUser {
                channel: "telegram".into(),
                is_direct: false,
            })
            .with_mention(MentionState::Mentioned)
    }

    fn agent_mention_message() -> IncomingMessage {
        IncomingMessage::new("user1", "conv1", "@sales I need help")
    }

    fn workspace_mention_message() -> IncomingMessage {
        IncomingMessage::new("user1", "conv1", "#team-alpha hello")
    }

    fn interrupt_message() -> IncomingMessage {
        IncomingMessage::new("user1", "conv1", "!stop")
    }

    fn image_message() -> IncomingMessage {
        let attachment = Attachment::new("cat.png", "image/png").with_data(vec![1, 2, 3, 4]);
        IncomingMessage::new("user1", "conv1", "look at this").with_attachment(attachment)
    }

    fn dispatch_config_suppress_groups() -> AutoReplyDispatchConfig {
        AutoReplyDispatchConfig {
            suppress_unless_mentioned_in_groups: true,
            ..Default::default()
        }
    }

    fn router_config_default_agent(agent_id: &str) -> AgentRouterConfig {
        AgentRouterConfig {
            default_agent_id: agent_id.into(),
            ..Default::default()
        }
    }

    async fn build_pipeline(
        config: AutoReplyDispatchConfig,
        router_config: AgentRouterConfig,
        debounce_ms: u64,
    ) -> PipelineHarness {
        let router = AgentRouter::new(router_config);
        build_pipeline_with_router(router, config, debounce_ms).await
    }

    async fn build_pipeline_with_router(
        router: AgentRouter,
        config: AutoReplyDispatchConfig,
        debounce_ms: u64,
    ) -> PipelineHarness {
        let (flush_tx, flush_rx) = mpsc::channel::<Vec<DebouncedItem>>(64);
        let debouncer = InboundDebouncer::new(
            InboundDebouncerConfig {
                debounce_ms,
                ..Default::default()
            },
            flush_tx,
        );
        let dispatch = AutoReplyDispatch::new(config);
        PipelineHarness::new(debouncer, flush_rx, dispatch, router, None).await
    }

    #[tokio::test]
    async fn plain_message_routes_to_default() {
        let pipeline = build_pipeline(
            AutoReplyDispatchConfig::default(),
            router_config_default_agent("default"),
            50,
        )
        .await;
        let (result, channel) = pipeline.process(plain_message()).await;
        assert!(result.is_none(), "plain messages are debounced");
        assert_eq!(channel.len(), 1);
        assert_eq!(channel[0].agent_id, "default");
    }

    #[tokio::test]
    async fn command_bypasses_debounce() {
        let pipeline = build_pipeline(
            AutoReplyDispatchConfig::default(),
            router_config_default_agent("default"),
            50,
        )
        .await;
        let (result, _channel) = pipeline.process(command_message()).await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().queue_mode, QueueMode::Normal);
    }

    #[tokio::test]
    async fn group_unmentioned_suppressed() {
        let pipeline = build_pipeline(
            dispatch_config_suppress_groups(),
            router_config_default_agent("default"),
            50,
        )
        .await;
        let (result, channel) = pipeline.process(group_unmentioned_message()).await;
        assert!(result.is_none());
        assert!(channel.is_empty());
    }

    #[tokio::test]
    async fn group_mentioned_allowed() {
        let pipeline = build_pipeline(
            dispatch_config_suppress_groups(),
            router_config_default_agent("default"),
            50,
        )
        .await;
        let (result, channel) = pipeline.process(mentioned_message()).await;
        assert!(result.is_none(), "mentioned group messages are still debounced");
        assert_eq!(channel.len(), 1);
        assert!(!channel[0].suppress_delivery);
    }

    #[tokio::test]
    async fn agent_mention_overrides_default() {
        let pipeline = build_pipeline(
            AutoReplyDispatchConfig::default(),
            router_config_default_agent("default"),
            50,
        )
        .await;
        let (result, channel) = pipeline.process(agent_mention_message()).await;
        assert!(result.is_none(), "agent-mention messages are debounced");
        assert_eq!(channel.len(), 1);
        assert_eq!(channel[0].agent_id, "sales");
    }

    #[tokio::test]
    async fn workspace_mention_sets_workspace() {
        let router_config = router_config_default_agent("default");
        let router = AgentRouter::new(router_config);
        router
            .set_workspace_default("team-alpha", "alpha-agent".to_string())
            .await;

        let (flush_tx, flush_rx) = mpsc::channel::<Vec<DebouncedItem>>(64);
        let debouncer = InboundDebouncer::new(
            InboundDebouncerConfig {
                debounce_ms: 50,
                ..Default::default()
            },
            flush_tx,
        );
        let dispatch = AutoReplyDispatch::new(AutoReplyDispatchConfig::default());
        let pipeline = PipelineHarness::new(debouncer, flush_rx, dispatch, router, None).await;

        let (result, channel) = pipeline.process(workspace_mention_message()).await;
        assert!(result.is_none(), "workspace-mention messages are debounced");
        assert_eq!(channel.len(), 1);
        assert_eq!(channel[0].workspace_id, Some("team-alpha".to_string()));
        assert_eq!(channel[0].agent_id, "alpha-agent");
    }

    #[tokio::test]
    async fn interrupt_sets_queue_mode() {
        let pipeline = build_pipeline(
            AutoReplyDispatchConfig::default(),
            router_config_default_agent("default"),
            50,
        )
        .await;
        let (result, channel) = pipeline.process(interrupt_message()).await;
        assert!(result.is_some());
        assert_eq!(result.as_ref().unwrap().queue_mode, QueueMode::Interrupt);
        assert_routed_message_eq(&result, &Some(channel[0].clone()), "process vs channel");
    }

    #[tokio::test]
    async fn image_attachment_produces_media_results() {
        let pipeline = build_pipeline(
            AutoReplyDispatchConfig::default(),
            router_config_default_agent("default"),
            50,
        )
        .await;
        let (result, channel) = pipeline.process(image_message()).await;
        assert!(result.is_none(), "messages with attachments are debounced");
        assert_eq!(channel.len(), 1);
        assert!(channel[0].media_results.is_some());
    }

    #[tokio::test]
    async fn flush_debounced_messages() {
        let pipeline = build_pipeline(
            AutoReplyDispatchConfig::default(),
            router_config_default_agent("default"),
            10_000,
        )
        .await;

        // Absorb a debounced message; it should not produce an immediate result.
        let (result, channel) = pipeline.process(plain_message()).await;
        assert!(result.is_none());
        assert!(channel.is_empty());

        // Flushing the conversation should emit the routed message.
        let (flushed, channel) = pipeline.flush("conv1").await;
        assert_eq!(flushed.len(), 1);
        assert_routed_message_eq(
            &Some(flushed[0].clone()),
            &Some(channel[0].clone()),
            "flush vs channel",
        );
    }

    #[tokio::test]
    async fn process_detailed_absorbed_for_plain_message() {
        let pipeline = build_pipeline(
            AutoReplyDispatchConfig::default(),
            router_config_default_agent("default"),
            10_000,
        )
        .await;
        let (outcome, channel) = pipeline.process_detailed(plain_message()).await;
        assert!(
            matches!(outcome, InboundProcessOutcome::Absorbed),
            "plain messages should be absorbed"
        );
        assert!(channel.is_empty());
    }

    #[tokio::test]
    async fn process_detailed_suppressed_for_group_unmentioned() {
        let pipeline = build_pipeline(
            dispatch_config_suppress_groups(),
            router_config_default_agent("default"),
            50,
        )
        .await;
        let (outcome, channel) = pipeline.process_detailed(group_unmentioned_message()).await;
        assert!(
            matches!(outcome, InboundProcessOutcome::Suppressed { .. }),
            "unmentioned group messages should be suppressed"
        );
        assert!(channel.is_empty());
    }

    #[tokio::test]
    async fn process_detailed_routed_for_command() {
        let pipeline = build_pipeline(
            AutoReplyDispatchConfig::default(),
            router_config_default_agent("default"),
            50,
        )
        .await;
        let (outcome, channel) = pipeline.process_detailed(command_message()).await;
        assert!(
            matches!(outcome, InboundProcessOutcome::Routed(_)),
            "commands should be routed immediately"
        );
        assert_eq!(channel.len(), 1);
    }

    /// A plain message is debounced, so the pipeline — not the caller — flushes
    /// it. A refusal found there has to reach the gateway, or the sender's
    /// message vanishes with no explanation: it goes to the denial sink.
    #[tokio::test]
    async fn an_unauthorized_sender_gets_a_denial_on_the_debounced_path() {
        let router = AgentRouter::new(router_config_default_agent("default"));
        router
            .set_agent_principals("default", vec!["someone-else".to_string()])
            .await;

        let (flush_tx, flush_rx) = mpsc::channel::<Vec<DebouncedItem>>(64);
        let debouncer = InboundDebouncer::new(
            InboundDebouncerConfig {
                debounce_ms: 50,
                ..Default::default()
            },
            flush_tx,
        );
        let (routed_tx, mut routed_rx) = mpsc::channel::<RoutedMessage>(64);
        let (denied_tx, mut denied_rx) = mpsc::channel(64);
        let pipeline = Arc::new(
            DefaultInboundPipeline::new(
                debouncer,
                MediaUnderstandingPipeline::new(),
                AutoReplyDispatch::new(AutoReplyDispatchConfig::default()),
                QueueModeResolver::new(),
                Arc::new(router),
                routed_tx,
                flush_rx,
            )
            .with_denied_sink(denied_tx),
        );
        pipeline.clone().start();

        let message = IncomingMessage::new("stranger", "conv-x", "hello")
            .with_provenance(InputProvenance::ExternalUser {
                channel: "telegram".into(),
                is_direct: true,
            })
            .with_channel_instance("tg_alice");
        let outcome = pipeline.process_detailed(message).await;
        assert!(
            matches!(outcome, InboundProcessOutcome::Absorbed),
            "a plain message is debounced"
        );

        let denied = tokio::time::timeout(Duration::from_secs(2), denied_rx.recv())
            .await
            .expect("the refusal should reach the sink")
            .expect("the sink is still open");
        assert_eq!(denied.agent_id, "default");
        // The caller needs the message to reply in its conversation.
        assert_eq!(denied.message.user_id.0, "stranger");
        assert_eq!(denied.message.conversation_id.0, "conv-x");
        assert_eq!(denied.message.channel_key(), Some("tg_alice"));
        // Nothing reached the agent.
        assert!(routed_rx.try_recv().is_err());
    }

    /// A message that bypasses the debouncer is routed inline, so its refusal
    /// comes back as the outcome — the caller answers it directly.
    #[tokio::test]
    async fn an_unauthorized_sender_gets_a_denied_outcome_on_the_inline_path() {
        let router = AgentRouter::new(router_config_default_agent("default"));
        router
            .set_agent_principals("default", vec!["someone-else".to_string()])
            .await;

        let pipeline =
            build_pipeline_with_router(router, AutoReplyDispatchConfig::default(), 50).await;
        // A command bypasses the debouncer, so its refusal is the outcome.
        let message = IncomingMessage::new("stranger", "conv-x", "/help")
            .with_provenance(InputProvenance::ExternalUser {
                channel: "telegram".into(),
                is_direct: true,
            })
            .with_channel_instance("tg_alice");
        let (outcome, channel) = pipeline.process_detailed(message).await;

        match outcome {
            InboundProcessOutcome::Denied { agent_id, message } => {
                assert_eq!(agent_id, "default");
                assert_eq!(message.user_id.0, "stranger");
                assert_eq!(message.channel_key(), Some("tg_alice"));
            }
            other => panic!("expected Denied, got {other:?}"),
        }
        assert!(channel.is_empty(), "nothing reached the agent");
    }

    #[tokio::test]
    async fn auto_start_starts_background_loop() {
        let (flush_tx, flush_rx) = mpsc::channel::<Vec<DebouncedItem>>(64);
        let debouncer = InboundDebouncer::new(
            InboundDebouncerConfig {
                debounce_ms: 50,
                ..Default::default()
            },
            flush_tx,
        );
        let dispatch = AutoReplyDispatch::new(AutoReplyDispatchConfig::default());
        let router = AgentRouter::new(router_config_default_agent("default"));
        let (routed_tx, mut routed_rx) = mpsc::channel::<RoutedMessage>(64);

        let pipeline = Arc::new(
            DefaultInboundPipeline::new(
                debouncer,
                MediaUnderstandingPipeline::new(),
                dispatch,
                QueueModeResolver::new(),
                Arc::new(router),
                routed_tx,
                flush_rx,
            )
            .with_auto_start(true),
        );
        pipeline.clone().start_if_configured();

        // Wait for the background loop to be running, then send a plain message.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let _ = pipeline.process(plain_message()).await;

        // The debouncer should flush automatically via the started background loop.
        let channel_messages = drain_with_timeout(&mut routed_rx, Duration::from_millis(500)).await;
        assert_eq!(channel_messages.len(), 1);
        assert_eq!(channel_messages[0].agent_id, "default");
    }
}
