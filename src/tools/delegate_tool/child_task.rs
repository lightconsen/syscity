//! Child task execution for the delegation tool.

use super::*;

/// Execute a child task using the provided agent, reporting outcomes to both
/// the local [`DelegationTracker`] and the shared [`SubagentRegistry`].
///
/// When a task store is attached, the child gets a `delegation_tasks` row and
/// the caller-supplied [`DelegationScope`] is threaded through its message
/// metadata so it can read and write shared state via the `task_state` tool.
///
/// `coordinator`, when present, advances pending handoffs under the child's
/// tree root after the child finishes.
///
/// `wake`, when present, wakes the parent with the child's outcome after the
/// child finishes (parent auto-wake, v2).
pub(crate) struct ChildTaskEnv {
    pub tracker: DelegationTracker,
    /// Shared iteration budget across the whole delegation tree.
    pub iterations: Arc<AtomicUsize>,
    /// The child's agent (absent for registry-only dry spawns).
    pub agent: Option<Arc<crate::agent::Agent>>,
    pub registry: Arc<SubagentRegistry>,
    pub store: Option<Arc<DelegationTaskStore>>,
    /// Where this child sits in the delegation tree.
    pub scope: DelegationScope,
    pub agent_id: String,
    pub coordinator: Option<Arc<DelegationCoordinator>>,
    pub wake: Option<Arc<DelegationWake>>,
    /// Session the `delegate` call ran in (`parent_id` of `spawn_child`): the
    /// user session for a tree root, `delegation:<parent_run_id>` for a
    /// delegated parent. Recorded on the row so push events can be routed to
    /// the client watching the root conversation.
    pub parent_session: Option<String>,
}

pub(crate) fn execute_child_task(
    child_id: String,
    task: TaskSpec,
    env: ChildTaskEnv,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    // Boxed (type-erased) future: `execute_child_task -> maybe_advance ->
    // successor spawn -> execute_child_task` is a genuinely cyclic call graph,
    // so a concrete `impl Future` would recurse in the type system (E0391).
    Box::pin(async move {
        let ChildTaskEnv {
            tracker,
            iterations,
            agent,
            registry,
            store,
            scope,
            agent_id,
            coordinator,
            wake,
            parent_session,
        } = env;

        tracker.update_status(&child_id, ChildStatus::Running).await;

        debug!("Child {} starting execution", child_id);

        // Row linkage: the tree root and the child's parent row come from the scope.
        let root_id = scope.root_id.clone();
        let depth = scope.depth;
        let parent_id = scope.parent_task_id.clone();

        // Create the shared-state row and record a start event.
        if let Some(store) = &store {
            let title: String = task.prompt.chars().take(120).collect();
            if let Err(e) = store
                .create_task(NewTask {
                    id: &child_id,
                    root_id: &root_id,
                    parent_id: parent_id.as_deref(),
                    depth,
                    agent_id: &agent_id,
                    title: &title,
                    parent_session: parent_session.as_deref(),
                })
                .await
            {
                warn!("Failed to create delegation task '{}': {}", child_id, e);
            }
            if let Err(e) = store
                .append_event(
                    &child_id,
                    &DelegationEvent::new(
                        &agent_id,
                        "started",
                        task.prompt.chars().take(80).collect::<String>(),
                    ),
                )
                .await
            {
                warn!("Failed to record start event for '{}': {}", child_id, e);
            }
        }

        // Captured on whichever completion path runs, then delivered to the
        // parent after the outcome has been persisted (parent auto-wake, v2).
        // Every completion path assigns it before the read below.
        let wake_message: Option<String>;

        if let Some(agent) = agent {
            // Create incoming message for the child task
            let message = crate::channels::IncomingMessage::new(
                format!("child:{}", child_id),
                format!("delegation:{}", child_id),
                &task.prompt,
            )
            .with_metadata(
                crate::channels::MessageMetadata::new()
                    .with_extra("child_id", child_id.clone())
                    .with_extra("output_format", task.output_format.clone().unwrap_or_default())
                    .with_extra("allowed_tools", task.allowed_tools.join(","))
                    .with_extra(
                        crate::delegation::DELEGATION_SCOPE_KEY,
                        serde_json::to_value(&scope).unwrap_or(serde_json::Value::Null),
                    ),
            );

            // Build a debug-logging progress callback so child tool activity
            // surfaces in logs even though there is no parent callback to forward to.
            // Per-round usage is also accumulated onto the task row here: the
            // child's `OutgoingMessage.usage` is only the last round, so the
            // row total must be built up round by round.
            let child_id_cb = child_id.clone();
            let store_cb = store.clone();
            let progress_cb: crate::agent::ProgressCallback = Arc::new(move |event| {
                let cid = child_id_cb.clone();
                let store_cb = store_cb.clone();
                Box::pin(async move {
                    match event {
                        crate::agent::ProgressEvent::ToolCalling { name, arguments } => {
                            debug!("Child {} calling tool {}: {}", cid, name, arguments);
                        }
                        crate::agent::ProgressEvent::ToolResult { name, result, .. } => {
                            debug!("Child {} tool {} result: {} chars", cid, name, result.len());
                        }
                        crate::agent::ProgressEvent::RoundUsage { usage } => {
                            if let Some(store) = &store_cb {
                                if let Err(e) =
                                    store.add_usage(&cid, usage.total_tokens as u64).await
                                {
                                    warn!("Child {} usage update failed: {}", cid, e);
                                }
                            }
                        }
                        crate::agent::ProgressEvent::Error { message } => {
                            warn!("Child {} progress error: {}", cid, message);
                        }
                        _ => {}
                    }
                })
            });

            // Process the task through the agent with progress visibility
            match agent
                .process_message_with_progress(message, progress_cb)
                .await
            {
                Ok(response) => {
                    iterations.fetch_add(1, Ordering::SeqCst);

                    info!(
                        "Child {} completed successfully. Response: {} chars",
                        child_id,
                        response.content.len()
                    );

                    // Format result based on output_format if specified
                    let result = if let Some(format) = &task.output_format {
                        format!("Output format ({}): {}", format, response.content)
                    } else {
                        response.content.clone()
                    };

                    tracker.set_result(&child_id, result.clone()).await;
                    registry.complete_run(&child_id, Ok(result.clone())).await;
                    wake_message = Some(child_completion_message(&child_id, &result));

                    // Write the outcome back to the shared task record.
                    if let Some(store) = &store {
                        // A task that requested a handoff must keep `waiting_handoff`
                        // so the coordinator can advance it.
                        preserve_handoff_and_set_status(store, &child_id, "completed").await;
                        if let Err(e) = store
                            .append_event(
                                &child_id,
                                &DelegationEvent::new(
                                    &agent_id,
                                    "completed",
                                    format!("output: {} chars", response.content.len()),
                                ),
                            )
                            .await
                        {
                            warn!("Failed to record completion event for '{}': {}", child_id, e);
                        }
                    }
                }
                Err(e) => {
                    error!("Child {} failed: {}", child_id, e);
                    let err_msg = format!("Task execution failed: {}", e);
                    tracker.set_error(&child_id, err_msg.clone()).await;
                    wake_message = Some(child_failure_message(&child_id, &err_msg));
                    registry.complete_run(&child_id, Err(err_msg)).await;

                    if let Some(store) = &store {
                        preserve_handoff_and_set_status(store, &child_id, "failed").await;
                    }
                }
            }
        } else {
            // No agent configured - log warning and mark as failed
            warn!(
                "No agent configured for child {}. Task would execute with prompt: {}",
                child_id, task.prompt
            );
            let err_msg = "No agent configured for delegation".to_string();
            tracker.set_error(&child_id, err_msg.clone()).await;
            wake_message = Some(child_failure_message(&child_id, &err_msg));
            registry.complete_run(&child_id, Err(err_msg)).await;

            if let Some(store) = &store {
                preserve_handoff_and_set_status(store, &child_id, "failed").await;
            }
        }

        // Wake the parent if it ended its turn with this child outstanding
        // (parent auto-wake, v2).  Runs after the outcome is persisted so the
        // parent can pull it via `delegate status` / `wait` if it prefers, and
        // the `parent_active_for_wake` guard consults the parent's own row.
        if let (Some(wake), Some(message)) = (&wake, wake_message) {
            notify_parent(&registry, store.as_deref(), wake, &child_id, &message).await;
        }

        // Drive any pending handoffs in this tree (successor continuation).  Runs
        // on both the success and failure paths so a handoff requested just before
        // a child finished is still picked up.
        if let Some(coordinator) = &coordinator {
            if let Err(e) = coordinator.maybe_advance(&root_id).await {
                warn!("Failed to advance delegation tree '{}': {}", root_id, e);
            }
        }

        debug!("Child {} execution completed", child_id);
    })
}

/// Set a delegation task's status unless it is already `waiting_handoff`.
///
/// A child that requested a handoff must keep `waiting_handoff` so the
/// coordinator can pick it up and spawn the successor; overwriting it with
/// `completed`/`failed` would silently drop the handoff.
async fn preserve_handoff_and_set_status(
    store: &DelegationTaskStore,
    child_id: &str,
    status: &str,
) {
    let pending_handoff = store
        .get_task(child_id)
        .await
        .ok()
        .flatten()
        .is_some_and(|task| task.is_waiting_handoff());
    if pending_handoff {
        return;
    }
    if let Err(e) = store.set_status(child_id, status).await {
        warn!("Failed to mark delegation task '{}' {}: {}", child_id, status, e);
    }
}
