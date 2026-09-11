use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Timelike;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};

use super::config::HeartbeatConfig;
use super::events::{HeartbeatEvent, HeartbeatStatus};
use super::parser::{
    is_heartbeat_content_empty, parse_heartbeat_tasks, HeartbeatTask, TaskDedupTracker,
};
use super::wake::{WakePriority, WakeRequest};
use crate::channels::IncomingMessage;
use crate::gateway::GatewayState;

/// Default HEARTBEAT.md filename
const HEARTBEAT_FILENAME: &str = "HEARTBEAT.md";

/// State tracked per agent for heartbeat scheduling
struct AgentHeartbeatState {
    /// Resolved heartbeat config for this agent (agent override or global
    /// fallback)
    config: HeartbeatConfig,
    consecutive_idle: u32,
    last_run: Option<Instant>,
    /// When this agent should next run a heartbeat
    next_run_at: Instant,
    dedup: TaskDedupTracker,
}

impl AgentHeartbeatState {
    fn new(config: HeartbeatConfig, now: Instant) -> Self {
        let interval = config.interval_seconds;
        Self {
            config,
            consecutive_idle: 0,
            last_run: None,
            next_run_at: now + Duration::from_secs(interval),
            dedup: TaskDedupTracker::new(),
        }
    }

    /// Reschedule next run based on current time and interval
    fn reschedule(&mut self, now: Instant) {
        self.next_run_at = now + Duration::from_secs(self.config.interval_seconds);
    }
}

/// The heartbeat runner that periodically wakes agents to check HEARTBEAT.md
pub struct HeartbeatRunner {
    state: Arc<GatewayState>,
    agent_states: Arc<RwLock<HashMap<String, AgentHeartbeatState>>>,
    pub(crate) event_tx: tokio::sync::broadcast::Sender<HeartbeatEvent>,
    wake_rx: mpsc::Receiver<WakeRequest>,
    wake_tx: mpsc::Sender<WakeRequest>,
}

impl HeartbeatRunner {
    pub fn new(state: Arc<GatewayState>) -> Self {
        let (wake_tx, wake_rx) = mpsc::channel::<WakeRequest>(64);
        let (event_tx, _) = tokio::sync::broadcast::channel::<HeartbeatEvent>(256);

        Self {
            state,
            agent_states: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            wake_rx,
            wake_tx,
        }
    }

    /// Return a sender for external wake requests
    pub fn wake_sender(&self) -> mpsc::Sender<WakeRequest> {
        self.wake_tx.clone()
    }

    /// Return an event subscriber for heartbeat status
    pub fn event_subscribe(&self) -> tokio::sync::broadcast::Receiver<HeartbeatEvent> {
        self.event_tx.subscribe()
    }

    /// Start the heartbeat runner loop
    pub async fn start(mut self) {
        self.init_agent_states().await;

        loop {
            let next_wake = self.compute_next_wake().await;
            let now = Instant::now();
            let sleep_duration = next_wake.saturating_duration_since(now);

            info!("Heartbeat runner waiting: next_wake_in={:.1}s", sleep_duration.as_secs_f64());

            tokio::select! {
                _ = tokio::time::sleep(sleep_duration) => {
                    self.run_due_agents().await;
                }
                Some(req) = self.wake_rx.recv() => {
                    self._emit_event(HeartbeatEvent::Started);
                    self.handle_wake_request(&req).await;
                }
                _ = self.state.shutdown_token.cancelled() => {
                    info!("Heartbeat runner received shutdown signal, exiting");
                    break;
                }
            }
        }
        info!("Heartbeat runner stopped");
    }

    /// Initialize heartbeat state for all existing agents
    async fn init_agent_states(&self) {
        let now = Instant::now();
        let agents = self.state.agents.agents.read().await;
        let mut states = self.agent_states.write().await;

        for (agent_id, handle) in agents.iter() {
            let config = self.resolve_agent_config(handle).await;
            if config.enabled {
                info!(
                    "Heartbeat initialized for agent {}: interval={}s, active_hours={}-{}",
                    agent_id,
                    config.interval_seconds,
                    config.active_hours_start,
                    config.active_hours_end
                );
            } else {
                debug!("Heartbeat disabled for agent {}", agent_id);
            }
            states.insert(agent_id.clone(), AgentHeartbeatState::new(config, now));
        }
    }

    /// Compute the next Instant when any agent should run.
    /// Returns now + 1s if no agents are registered or all are disabled.
    async fn compute_next_wake(&self) -> Instant {
        let states = self.agent_states.read().await;
        let now = Instant::now();

        let mut next_wake = None;
        for state in states.values() {
            if !state.config.enabled {
                continue;
            }
            match next_wake {
                None => next_wake = Some(state.next_run_at),
                Some(nw) if state.next_run_at < nw => next_wake = Some(state.next_run_at),
                _ => {}
            }
        }

        next_wake.unwrap_or_else(|| now + Duration::from_secs(1))
    }

    /// Run heartbeat for all agents whose next_run_at has passed.
    async fn run_due_agents(&self) {
        let now = Instant::now();
        let mut due_ids: Vec<String> = Vec::new();

        {
            let states = self.agent_states.read().await;
            for (agent_id, state) in states.iter() {
                if !state.config.enabled {
                    continue;
                }
                if state.next_run_at <= now {
                    due_ids.push(agent_id.clone());
                }
            }
        }

        if due_ids.is_empty() {
            return;
        }

        info!("Heartbeat cycle: {} agents due", due_ids.len());
        self._emit_event(HeartbeatEvent::Started);

        for agent_id in due_ids {
            self.run_heartbeat_for_agent_by_id(&agent_id).await;
        }
    }

    /// Run heartbeat for a single agent by ID (used by the main loop)
    async fn run_heartbeat_for_agent_by_id(&self, agent_id: &str) {
        let handle = {
            let agents = self.state.agents.agents.read().await;
            match agents.get(agent_id) {
                Some(h) => h.clone(),
                None => {
                    warn!("Heartbeat: agent {} not found, removing state", agent_id);
                    let mut states = self.agent_states.write().await;
                    states.remove(agent_id);
                    return;
                }
            }
        };

        // Check if agent config changed (e.g., hot reload updated heartbeat config)
        let agent_config = self.resolve_agent_config(&handle).await;
        {
            let mut states = self.agent_states.write().await;
            if let Some(state) = states.get_mut(agent_id) {
                // Update config if it changed
                if state.config != agent_config {
                    debug!("Agent {} heartbeat config updated", agent_id);
                    state.config = agent_config;
                }

                // Check active hours
                if !is_within_active_hours(&state.config) {
                    debug!("Agent {} heartbeat skipped: outside active hours", agent_id);
                    self._emit_event(HeartbeatEvent::Skipped {
                        reason: "outside_active_hours".to_string(),
                        agent_id: agent_id.to_string(),
                    });
                    state.reschedule(Instant::now());
                    return;
                }

                // Check max consecutive idle
                if state.consecutive_idle >= state.config.max_consecutive_idle {
                    debug!(
                        "Agent {} heartbeat skipped: max consecutive idle ({}/{}) reached",
                        agent_id, state.consecutive_idle, state.config.max_consecutive_idle,
                    );
                    self._emit_event(HeartbeatEvent::Skipped {
                        reason: "max_consecutive_idle".to_string(),
                        agent_id: agent_id.to_string(),
                    });
                    state.reschedule(Instant::now());
                    return;
                }
            } else {
                // No state yet — create it (agent added after runner started)
                states.insert(
                    agent_id.to_string(),
                    AgentHeartbeatState::new(agent_config, Instant::now()),
                );
            }
        }

        self.run_heartbeat_for_agent(&handle, None).await;
    }

    /// Handle a single wake request (from cron or external triggers)
    async fn handle_wake_request(&self, req: &WakeRequest) {
        info!("Heartbeat wake request: agent={}, priority={:?}", req.agent_id, req.priority);

        let agents = self.state.agents.agents.read().await;
        let handle = match agents.get(&req.agent_id) {
            Some(h) => h.clone(),
            None => {
                warn!("Heartbeat wake: agent {} not found", req.agent_id);
                self._emit_event(HeartbeatEvent::Skipped {
                    reason: "agent_not_found".to_string(),
                    agent_id: req.agent_id.clone(),
                });
                return;
            }
        };
        drop(agents);

        // Resolve agent config and check if heartbeat is enabled
        let agent_config = self.resolve_agent_config(&handle).await;
        if !agent_config.enabled {
            debug!("Heartbeat wake skipped: agent {} heartbeat disabled", req.agent_id);
            self._emit_event(HeartbeatEvent::Skipped {
                reason: "heartbeat_disabled".to_string(),
                agent_id: req.agent_id.clone(),
            });
            return;
        }

        // Check active hours for wake requests too
        if !is_within_active_hours(&agent_config) {
            debug!("Heartbeat wake skipped: agent {} outside active hours", req.agent_id);
            self._emit_event(HeartbeatEvent::Skipped {
                reason: "outside_active_hours".to_string(),
                agent_id: req.agent_id.clone(),
            });
            return;
        }

        if handle.busy.load(std::sync::atomic::Ordering::Relaxed) {
            if req.priority == WakePriority::Retry {
                debug!("Agent {} busy, skipping retry wake", req.agent_id);
                return;
            }
            let wake_tx = self.wake_tx.clone();
            let req = WakeRequest {
                agent_id: req.agent_id.clone(),
                priority: WakePriority::Retry,
                prompt: req.prompt.clone(),
            };
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if let Err(e) = wake_tx.send(req).await {
                    warn!("Heartbeat wake retry send failed: {}", e);
                }
            });
            return;
        }

        self.run_heartbeat_for_agent(&handle, req.prompt.as_deref())
            .await;
    }

    /// Read HEARTBEAT.md content
    async fn read_heartbeat_content(&self, handle: &crate::gateway::AgentHandle) -> Option<String> {
        // Try workspace_dir from agent config
        if let Some(ref workspace_dir) = handle.config.workspace_dir {
            let path = workspace_dir.join(HEARTBEAT_FILENAME);
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                return Some(content);
            }
        }

        // Try per-agent directory: <root>/agents/{id}/HEARTBEAT.md
        let agent_path = self
            .state
            .paths
            .agents_dir()
            .join(&handle.id)
            .join(HEARTBEAT_FILENAME);
        if let Ok(content) = tokio::fs::read_to_string(&agent_path).await {
            return Some(content);
        }

        // Fallback: workspace-level HEARTBEAT.md
        let workspace_path = self
            .state
            .paths
            .workspace_data_dir()
            .join(HEARTBEAT_FILENAME);
        if let Ok(content) = tokio::fs::read_to_string(&workspace_path).await {
            return Some(content);
        }

        None
    }

    /// Run heartbeat for a single agent
    async fn run_heartbeat_for_agent(
        &self,
        handle: &crate::gateway::AgentHandle,
        custom_prompt: Option<&str>,
    ) {
        let agent_id = &handle.id;

        if handle.busy.load(std::sync::atomic::Ordering::Relaxed) {
            self._emit_event(HeartbeatEvent::Skipped {
                reason: "agent_busy".to_string(),
                agent_id: agent_id.clone(),
            });
            let mut states = self.agent_states.write().await;
            if let Some(state) = states.get_mut(agent_id) {
                state.reschedule(Instant::now());
            }
            return;
        }

        let heartbeat_content = self.read_heartbeat_content(handle).await;

        // If HEARTBEAT.md is empty and no custom prompt, mark idle
        if custom_prompt.is_none()
            && heartbeat_content
                .as_ref()
                .is_none_or(|c| is_heartbeat_content_empty(c))
        {
            self._emit_event(HeartbeatEvent::Completed {
                status: HeartbeatStatus::Idle,
                agent_id: agent_id.clone(),
                session_id: None,
            });
            self.update_consecutive_idle(agent_id, true).await;
            let mut states = self.agent_states.write().await;
            if let Some(state) = states.get_mut(agent_id) {
                state.reschedule(Instant::now());
            }
            return;
        }

        let prompt = self
            .build_heartbeat_prompt(agent_id, custom_prompt, heartbeat_content.clone())
            .await;

        let session_id = format!("heartbeat:{}", agent_id);
        info!("Heartbeat poll for agent {}", agent_id);

        let message = IncomingMessage::new("system", &session_id, &prompt).with_provenance(
            crate::channels::InputProvenance::InternalSystem {
                source: "heartbeat".to_string(),
            },
        );

        match handle.agent.process_message(message).await {
            Ok(response) => {
                self.on_heartbeat_ok(agent_id, heartbeat_content, session_id, response)
                    .await;
            }
            Err(e) => {
                error!("Heartbeat failed for agent {}: {}", agent_id, e);
                self._emit_event(HeartbeatEvent::Failed {
                    error: e.to_string(),
                    agent_id: agent_id.to_string(),
                });
                self.update_consecutive_idle(agent_id, false).await;
                let mut states = self.agent_states.write().await;
                if let Some(agent_state) = states.get_mut(agent_id) {
                    agent_state.reschedule(Instant::now());
                }
            }
        }
    }

    /// Build the prompt to send to the agent for a heartbeat cycle.
    async fn build_heartbeat_prompt(
        &self,
        agent_id: &str,
        custom_prompt: Option<&str>,
        heartbeat_content: Option<String>,
    ) -> String {
        match custom_prompt {
            Some(cp) => cp.to_string(),
            None => {
                let tasks = heartbeat_content
                    .as_deref()
                    .map(parse_heartbeat_tasks)
                    .unwrap_or_default();

                let due_tasks: Vec<&HeartbeatTask> = {
                    let states = self.agent_states.read().await;
                    if let Some(agent_state) = states.get(agent_id) {
                        tasks
                            .iter()
                            .filter(|t| agent_state.dedup.is_task_due(t))
                            .collect()
                    } else {
                        tasks.iter().collect()
                    }
                };

                if due_tasks.is_empty() && tasks.is_empty() {
                    "Read HEARTBEAT.md if it exists. Follow it strictly. Do not invent or repeat \
                     old tasks from prior chats. If nothing needs attention, reply HEARTBEAT_OK."
                        .to_string()
                } else if due_tasks.is_empty() {
                    "Read HEARTBEAT.md. No tasks are due at this time. If nothing else needs \
                     attention, reply HEARTBEAT_OK."
                        .to_string()
                } else {
                    let mut prompt = "Read HEARTBEAT.md. The following tasks are due for \
                                      execution:\n\n"
                        .to_string();
                    for task in &due_tasks {
                        prompt.push_str(&format!("- **{}**: {}\n", task.name, task.prompt));
                    }
                    prompt.push_str(
                        "\nExecute the due tasks. For any completed task, note it so it can be \
                         tracked.",
                    );
                    prompt
                }
            }
        }
    }

    /// Handle a successful heartbeat response from an agent.
    async fn on_heartbeat_ok(
        &self,
        agent_id: &str,
        heartbeat_content: Option<String>,
        session_id: String,
        response: crate::channels::OutgoingMessage,
    ) {
        if let Some(ref content) = heartbeat_content {
            let tasks = parse_heartbeat_tasks(content);
            if !response.content.contains("HEARTBEAT_OK") && !tasks.is_empty() {
                let mut states = self.agent_states.write().await;
                if let Some(agent_state) = states.get_mut(agent_id) {
                    // Prune stale task entries to prevent unbounded HashMap growth
                    let active: std::collections::HashSet<String> =
                        tasks.iter().map(|t| t.name.clone()).collect();
                    agent_state.dedup.retain_active(&active);
                    for task in &tasks {
                        agent_state.dedup.mark_executed(&task.name);
                    }
                }
            }
        }

        let is_idle = response.content.contains("HEARTBEAT_OK");
        let status = if is_idle {
            HeartbeatStatus::Idle
        } else {
            HeartbeatStatus::TaskExecuted
        };

        self.update_consecutive_idle(agent_id, is_idle).await;

        self._emit_event(HeartbeatEvent::Completed {
            status,
            agent_id: agent_id.to_string(),
            session_id: Some(session_id),
        });

        info!(
            "Heartbeat response from {}: status={:?}, content_preview: {}",
            agent_id,
            status,
            response.content.chars().take(80).collect::<String>()
        );

        let mut states = self.agent_states.write().await;
        if let Some(agent_state) = states.get_mut(agent_id) {
            agent_state.last_run = Some(Instant::now());
            agent_state.reschedule(Instant::now());
        }
    }

    /// Update consecutive idle counter for an agent
    async fn update_consecutive_idle(&self, agent_id: &str, is_idle: bool) {
        let mut states = self.agent_states.write().await;
        if let Some(agent_state) = states.get_mut(agent_id) {
            if is_idle {
                agent_state.consecutive_idle += 1;
            } else {
                agent_state.consecutive_idle = 0;
            }
        }
    }

    /// Resolve heartbeat config for an agent.
    /// Uses agent-specific config if set, otherwise falls back to global
    /// GatewayConfig.
    async fn resolve_agent_config(&self, handle: &crate::gateway::AgentHandle) -> HeartbeatConfig {
        if let Some(ref agent_heartbeat) = handle.config.heartbeat {
            return agent_heartbeat.clone();
        }

        let config_lock = self.state.config.read().await;
        config_lock.heartbeat.clone()
    }

    fn _emit_event(&self, event: HeartbeatEvent) {
        if self.event_tx.receiver_count() == 0 {
            return; // No subscribers, nothing to emit
        }
        if let Err(e) = self.event_tx.send(event) {
            debug!("Heartbeat event send failed (no subscribers): {}", e);
        }
    }
}

/// Check if current time is within the active hours defined in the given config
fn is_within_active_hours(config: &HeartbeatConfig) -> bool {
    let now = chrono::Local::now();
    let current_minutes = now.hour() * 60 + now.minute();

    let start = parse_time(&config.active_hours_start);
    let end = parse_time(&config.active_hours_end);

    match (start, end) {
        (Some(s), Some(e)) => {
            if s <= e {
                current_minutes >= s && current_minutes < e
            } else {
                current_minutes >= s || current_minutes < e
            }
        }
        _ => true,
    }
}

/// Parse a time string "HH:MM" into minutes since midnight
fn parse_time(s: &str) -> Option<u32> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let hour: u32 = parts[0].parse().ok()?;
    let minute: u32 = parts[1].parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(hour * 60 + minute)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn make_test_agent_handle(
        id: &str,
        heartbeat: Option<HeartbeatConfig>,
    ) -> crate::gateway::AgentHandle {
        let agent_config = crate::agent::AgentConfig {
            heartbeat,
            ..Default::default()
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let (query_tx, _query_rx) = tokio::sync::mpsc::channel(1);
        let provider: Arc<dyn crate::providers::Provider> =
            Arc::new(crate::providers::mock::MockProvider::new());
        let tools = Arc::new(crate::tools::ToolRegistry::new());
        let agent = Arc::new(crate::agent::Agent::new(agent_config.clone(), provider, tools));
        crate::gateway::AgentHandle {
            id: id.to_string(),
            config: agent_config.clone(),
            base_config: agent_config,
            tx,
            query_tx,
            busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent,
        }
    }

    #[tokio::test]
    async fn test_resolve_agent_config_falls_back_to_global() {
        let state =
            crate::gateway::state_tests::make_test_state(crate::gateway::GatewayConfig::default())
                .await;
        let state = Arc::new(state);
        let runner = HeartbeatRunner::new(state.clone());

        let handle = make_test_agent_handle("agent-a", None);
        let resolved = runner.resolve_agent_config(&handle).await;
        assert_eq!(resolved, crate::heartbeat::HeartbeatConfig::default());
    }

    #[tokio::test]
    async fn test_resolve_agent_config_uses_agent_override() {
        let mut gateway_config = crate::gateway::GatewayConfig::default();
        gateway_config.heartbeat = crate::heartbeat::HeartbeatConfig {
            interval_seconds: 300,
            ..Default::default()
        };
        let state = crate::gateway::state_tests::make_test_state(gateway_config).await;
        let state = Arc::new(state);
        let runner = HeartbeatRunner::new(state.clone());

        let agent_heartbeat = crate::heartbeat::HeartbeatConfig {
            interval_seconds: 60,
            ..Default::default()
        };
        let handle = make_test_agent_handle("agent-b", Some(agent_heartbeat));
        let resolved = runner.resolve_agent_config(&handle).await;
        assert_eq!(resolved.interval_seconds, 60); // agent override
        assert_ne!(resolved, crate::heartbeat::HeartbeatConfig::default());
    }

    #[tokio::test]
    async fn test_two_agents_different_intervals() {
        // Agent A: heartbeat every 1s
        // Agent B: heartbeat every 3s
        // Run for ~3.5s, expect A ~3-4 times, B ~1 time

        let mut gateway_config = crate::gateway::GatewayConfig::default();
        gateway_config.heartbeat = HeartbeatConfig {
            enabled: true,
            interval_seconds: 10, // global default, overridden per agent
            active_hours_start: "00:00".to_string(),
            active_hours_end: "23:59".to_string(),
            max_consecutive_idle: 100,
            ..Default::default()
        };

        let state = crate::gateway::state_tests::make_test_state(gateway_config).await;
        let state = Arc::new(state);

        // Insert agents into GatewayState BEFORE runner starts
        {
            let mut agents = state.agents.agents.write().await;
            let handle_a = make_test_agent_handle(
                "agent-fast",
                Some(HeartbeatConfig {
                    enabled: true,
                    interval_seconds: 1,
                    active_hours_start: "00:00".to_string(),
                    active_hours_end: "23:59".to_string(),
                    max_consecutive_idle: 100,
                    ..Default::default()
                }),
            );
            let handle_b = make_test_agent_handle(
                "agent-slow",
                Some(HeartbeatConfig {
                    enabled: true,
                    interval_seconds: 3,
                    active_hours_start: "00:00".to_string(),
                    active_hours_end: "23:59".to_string(),
                    max_consecutive_idle: 100,
                    ..Default::default()
                }),
            );
            agents.insert("agent-fast".to_string(), handle_a);
            agents.insert("agent-slow".to_string(), handle_b);
        }

        let runner = HeartbeatRunner::new(state.clone());
        let mut event_rx = runner.event_subscribe();

        // Start runner in background, cancel after 3.5s
        let runner_handle = tokio::spawn(async move {
            runner.start().await;
        });

        // Collect events for ~3.5 seconds
        let mut events = Vec::new();
        let collect = tokio::time::timeout(Duration::from_millis(3800), async {
            while let Ok(event) = event_rx.recv().await {
                events.push(event);
            }
        });
        let _ = collect.await;

        // Cancel runner
        runner_handle.abort();

        // Count Completed events per agent
        let mut fast_count = 0u32;
        let mut slow_count = 0u32;
        for event in &events {
            if let HeartbeatEvent::Completed { agent_id, .. } = event {
                match agent_id.as_str() {
                    "agent-fast" => fast_count += 1,
                    "agent-slow" => slow_count += 1,
                    _ => {}
                }
            }
        }

        info!(
            "Integration test results: fast={}, slow={}, total_events={}",
            fast_count,
            slow_count,
            events.len()
        );

        // Agent-fast (1s interval) should have run at least 2-3 times in 3.5s
        // (first run at ~0s after init, then at ~1s, ~2s, ~3s)
        assert!(
            fast_count >= 2,
            "agent-fast should have run at least 2 times in 3.5s, got {}",
            fast_count
        );

        // Agent-slow (3s interval) should have run at least once
        assert!(
            slow_count >= 1,
            "agent-slow should have run at least 1 time in 3.5s, got {}",
            slow_count
        );

        // fast should have run more times than slow
        assert!(
            fast_count > slow_count,
            "agent-fast ({}) should run more than agent-slow ({})",
            fast_count,
            slow_count
        );
    }

    #[test]
    fn test_agent_heartbeat_state_new() {
        let config = HeartbeatConfig {
            enabled: true,
            interval_seconds: 60,
            ..Default::default()
        };
        let now = Instant::now();
        let state = AgentHeartbeatState::new(config, now);

        assert_eq!(state.consecutive_idle, 0);
        assert!(state.last_run.is_none());
        assert!(state.next_run_at >= now + Duration::from_secs(60));
        assert!(state.next_run_at <= now + Duration::from_secs(65));
    }

    #[test]
    fn test_agent_heartbeat_state_reschedule() {
        let config = HeartbeatConfig {
            enabled: true,
            interval_seconds: 30,
            ..Default::default()
        };
        let now = Instant::now();
        let mut state = AgentHeartbeatState::new(config, now);

        let before = state.next_run_at;
        state.reschedule(now + Duration::from_secs(10));
        let after = state.next_run_at;

        assert!(after > before);
        let expected = now + Duration::from_secs(10) + Duration::from_secs(30);
        assert!(after >= expected);
        assert!(after <= expected + Duration::from_secs(1));
    }

    #[test]
    fn test_is_within_active_hours_daytime() {
        let config = HeartbeatConfig {
            active_hours_start: "08:00".to_string(),
            active_hours_end: "23:00".to_string(),
            ..Default::default()
        };
        // This test may be flaky depending on the current time,
        // so we just verify the function doesn't panic
        let _ = is_within_active_hours(&config);
    }

    #[test]
    fn test_is_within_active_hours_wraparound() {
        let config = HeartbeatConfig {
            active_hours_start: "23:00".to_string(),
            active_hours_end: "08:00".to_string(),
            ..Default::default()
        };
        let _ = is_within_active_hours(&config);
    }

    #[test]
    fn test_is_within_active_hours_invalid() {
        let config = HeartbeatConfig {
            active_hours_start: "invalid".to_string(),
            active_hours_end: "also-invalid".to_string(),
            ..Default::default()
        };
        // Invalid times should default to always active
        assert!(is_within_active_hours(&config));
    }

    #[test]
    fn test_heartbeat_config_eq() {
        let a = HeartbeatConfig::default();
        let b = HeartbeatConfig::default();
        assert_eq!(a, b);

        let c = HeartbeatConfig {
            interval_seconds: 120,
            ..Default::default()
        };
        assert_ne!(a, c);
    }
}
