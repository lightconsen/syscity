//! Gateway shutdown: `stop_gateway` and the task-class predicate it uses.

use super::*;

// ── stop ─────────────────────────────────────────────────────────────

/// Gracefully shut down the gateway and its subsystems.
pub(crate) async fn stop_gateway(
    shutdown_token: &CancellationToken,
    state: &Arc<GatewayState>,
) -> crate::Result<()> {
    info!("Shutting down Syscity Gateway...");

    // Signal every cancel-aware loop to exit.
    shutdown_token.cancel();

    // 1. Drain the unified message workers.
    // Abort all first (shutdown_token was already cancelled), then await
    // so we don't hang on stuck tasks.
    let message_handles = state
        .task_registry
        .remove_matching_join_or_abort("message:")
        .await;
    for handle in &message_handles {
        handle.abort();
    }
    for handle in message_handles {
        let _ = handle.await;
    }

    // 2. Stop all spawned agents and await their loops.
    {
        let agents = state.agents.agents.read().await;
        for (id, handle) in agents.iter() {
            if let Err(e) = handle.tx.send(crate::gateway::AgentCommand::Shutdown).await {
                warn!("Failed to send shutdown to agent {}: {}", id, e);
            }
        }
    }
    let agent_handles = state
        .task_registry
        .remove_matching_join_or_abort("agent:")
        .await;
    for handle in &agent_handles {
        handle.abort();
    }
    for handle in agent_handles {
        let _ = handle.await;
    }

    // 2b. Abort running goal runners and their event relays ("goal:{id}" /
    // "goal-relay:{id}"). Shutdown must NOT take the cooperative-cancel
    // path: /goal cancel deletes the checkpoint (the user explicitly
    // discards the goal), while shutdown is crash-equivalent — the last
    // round checkpoint survives and the goal shows up as suspended after
    // restart.
    let goal_handles = state
        .task_registry
        .remove_matching_join_or_abort("goal")
        .await;
    for handle in &goal_handles {
        handle.abort();
    }
    for handle in goal_handles {
        let _ = handle.await;
    }

    // 3. Stop configured channels.
    // Abort channel background tasks first so the channel stop() calls do not
    // race with gateway-owned inbound/outbound bridges.
    let channel_handles = state
        .task_registry
        .remove_matching_join_or_abort("channel:")
        .await;
    for handle in channel_handles {
        handle.abort();
    }
    let channel_refs: Vec<Arc<dyn crate::channels::Channel>> = {
        let channels = state.channels.channels.read().await;
        channels.values().cloned().collect()
    };
    for channel in channel_refs {
        let name = channel.name().to_string();
        if let Err(e) = channel.stop().await {
            warn!("Failed to stop channel '{}': {}", name, e);
        } else {
            info!("Channel '{}' stopped", name);
        }
    }

    // 4. ACP shutdown.
    if let Err(e) = state.agents.acp.shutdown().await {
        warn!("Failed to shut down ACP control plane: {}", e);
    } else {
        info!("ACP control plane shut down");
    }

    // 5. Cron scheduler.
    if let Some(cron_arc) = state.scheduler.cron_scheduler.read().await.clone() {
        let mut scheduler = cron_arc.lock().await;
        if let Err(e) = scheduler.shutdown().await {
            warn!("Failed to shutdown cron scheduler: {}", e);
        } else {
            info!("Cron scheduler stopped");
        }
    }

    // 6. Dream scheduler.
    if let Some(mut scheduler) = state.memory.dream_scheduler.write().await.take() {
        scheduler.stop().await;
        if let Some(handle) = state
            .task_registry
            .remove_join_or_abort("dream_scheduler")
            .await
        {
            match timeout(Duration::from_secs(5), handle).await {
                Ok(_) => info!("Dream scheduler stopped"),
                Err(_) => warn!("Dream scheduler did not stop within timeout"),
            }
        } else {
            info!("Dream scheduler stopped");
        }
    }

    // 7. Standing orders manager.
    if let Some(mut manager) = state.memory.standing_order_manager.write().await.take() {
        manager.stop().await;
        info!("Standing orders manager stopped");
    }

    // 8. Disconnect MCP servers.
    let mcp_servers = state.tools.mcp_manager.list_servers().await;
    for server_id in mcp_servers {
        if let Err(e) = state.tools.mcp_manager.disconnect(&server_id).await {
            warn!("Failed to disconnect MCP server '{}': {}", server_id, e);
        }
    }

    // 9. Hot reload.
    if let Some(hot_reload) = state.infra.hot_reload.read().await.clone() {
        if let Err(e) = hot_reload.stop().await {
            warn!("Failed to stop hot reload manager: {}", e);
        }
    }

    // 10. Task scheduler.
    if let Some(ts_arc) = state.scheduler.task_scheduler.read().await.clone() {
        let mut scheduler = ts_arc.lock().await;
        if let Err(e) = scheduler.stop().await {
            warn!("Failed to stop task scheduler: {}", e);
        }
    }

    // 11. Browser bridge / pool.
    #[cfg(feature = "browser")]
    {
        let mut bridge_lock = state.infra.browser_bridge.write().await;
        if let Some(bridge) = bridge_lock.take() {
            bridge.shutdown().await;
            info!("Browser pool shut down");
        }
    }

    // 12. Stop what is left in the registry (includes followup timers now that
    //     they live in the unified registry).
    //
    //     Two kinds of task are still here. A writer with a short life left —
    //     a hook running, an audit row being inserted, a retention sweep
    //     mid-delete — has already seen `shutdown_token.cancel()` at the top of
    //     this function, so give it a bounded window to land that write rather
    //     than cutting it off at its next await point. A task whose lifetime is
    //     a client's socket ends when the peer goes away, which is not
    //     something shutdown can wait for; waiting on those would only ever
    //     burn the whole window and then report a healthy connection as a
    //     straggler, so they are aborted outright.
    let current_task = tokio::task::try_id();
    let mut draining: Vec<(String, JoinHandle<()>)> = Vec::new();
    let mut aborted: Vec<String> = Vec::new();
    for (name, task) in state.task_registry.take_all().await {
        let handle = match task {
            Task::Join(handle) => handle,
            Task::Abort(handle) => {
                handle.abort();
                aborted.push(name);
                continue;
            }
        };
        if Some(handle.id()) == current_task {
            // A restart task *is* the caller of this function: it cannot
            // finish while these lines run, and aborting it would cancel the
            // very future that reaches its `process::exit`. Leave it detached
            // to unwind on its own.
            info!("Shutdown is running inside task '{}'; leaving it detached", name);
        } else if is_socket_lifetime_task(&name) {
            handle.abort();
            aborted.push(name);
        } else {
            draining.push((name, handle));
        }
    }

    if !aborted.is_empty() {
        // Handle-only entries cannot be awaited by design, and a socket task
        // cannot be waited for; both are aborted outright.
        debug!("Aborted {} task(s) at shutdown: {}", aborted.len(), aborted.join(", "));
    }

    let drain = futures::future::join_all(draining.iter_mut().map(|(_, handle)| handle));
    match timeout(BACKGROUND_DRAIN_TIMEOUT, drain).await {
        Ok(results) => {
            for (result, (name, _)) in results.iter().zip(&draining) {
                if let Err(e) = result {
                    if e.is_panic() {
                        warn!("Background task '{}' panicked during shutdown: {}", name, e);
                    }
                }
            }
            info!("Drained {} background task(s)", draining.len());
        }
        Err(_) => {
            // A task that ignores the shutdown token, or is stuck somewhere
            // cancellation cannot reach, must not hold the process open.
            let mut stuck = Vec::new();
            for (name, handle) in &draining {
                if !handle.is_finished() {
                    handle.abort();
                    stuck.push(name.as_str());
                }
            }
            if stuck.is_empty() {
                // The window closed on the same poll that finished the last
                // task; nothing was actually cut short.
                info!("Drained {} background task(s)", draining.len());
            } else {
                warn!(
                    "{} background task(s) did not stop within {:?} and were aborted: {}",
                    stuck.len(),
                    BACKGROUND_DRAIN_TIMEOUT,
                    stuck.join(", ")
                );
            }
        }
    }

    // 13. Plugin manager shutdown.
    if let Err(e) = state.infra.plugin_manager.shutdown().await {
        warn!("Failed to shutdown plugin manager: {}", e);
    }

    // 13b. Writes the engine still owes the database.
    //
    //      Turn, thread, sample and session-row persistence runs on
    //      fire-and-forget tasks (`agent::writes`), so the registry drain above
    //      cannot see them — they are not in it by design, since those tasks
    //      must finish rather than be cancelled. Closing the pool under them
    //      loses a turn that the in-memory state already calls complete.
    if !crate::agent::writes::pending()
        .wait_idle(PENDING_WRITES_TIMEOUT)
        .await
    {
        warn!(
            "{} engine write(s) still in flight after {:?}; closing storage anyway",
            crate::agent::writes::pending().in_flight(),
            PENDING_WRITES_TIMEOUT
        );
    }

    // 14. Flush durable state, rather than leaving it to process exit — the
    //     `/restart` path ends in `std::process::exit`, which runs no
    //     destructors at all.
    //
    //     This is why the close comes after the drain and not before:
    //     `SqlitePool::close()` waits for every checked-out connection to be
    //     returned, so closing while a tracked task is still mid-query blocks
    //     here instead of flushing.
    match timeout(STORAGE_CLOSE_TIMEOUT, state.infra.storage.read().await.close()).await {
        Ok(Ok(())) => info!("Storage flushed and closed"),
        Ok(Err(e)) => warn!("Failed to close storage at shutdown: {}", e),
        Err(_) => warn!("Storage did not close within {:?}", STORAGE_CLOSE_TIMEOUT),
    }

    info!("Gateway shutdown complete");
    Ok(())
}

/// Tasks whose lifetime is a client's socket rather than a unit of work.
///
/// The registry holds tasks for the gateway's own loops and for per-connection
/// pumps alike, but only the former can be waited for: a socket task ends when
/// its peer goes away, so [`stop_gateway`] aborts these instead of spending its
/// drain window on them.
pub(super) fn is_socket_lifetime_task(name: &str) -> bool {
    name.starts_with("ws:") || name.starts_with("openai:sse:")
}
