//! Startup helpers: agent spawning, the quality gate and MCP wiring.

use super::*;

// ── Helpers ──────────────────────────────────────────────────────────

/// Spawn an agent and track its task handle.
pub(super) async fn spawn_agent_in_lifecycle(
    state: Arc<GatewayState>,
    id: String,
    config: AgentConfig,
) -> crate::Result<()> {
    spawn_agent_inner(state, id, config).await?;
    Ok(())
}

/// Run the quality gate check during startup.
///
/// Creates a temporary eval agent, runs all configured suites, and evaluates
/// criteria. If the gate fails, the error message includes details of which
/// criteria did not pass. Gate `shutdown_on_failure` determines whether this
/// error is fatal (blocking startup) or just a warning.
pub(super) async fn run_quality_gate_check(
    state: Arc<GatewayState>,
    config: &GatewayConfig,
) -> crate::Result<()> {
    info!("═══ Quality Gate: {} ═══", config.quality_gate.name);

    // 1. Resolve provider from config
    let provider_type = config.model_provider.clone();
    let api_key = match config.providers.get(&provider_type) {
        Some(p) => {
            let key = p.effective_key(Some(&state.secrets)).await;
            if key.is_empty() {
                None
            } else {
                Some(key)
            }
        }
        None => None,
    }
    .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
    .or_else(|| std::env::var("OPENAI_API_KEY").ok());

    let base_url = config
        .providers
        .get(&provider_type)
        .and_then(|p| p.base_url.clone())
        .or_else(|| std::env::var("SYSCITY_BASE_URL").ok());

    let model = Some(config.model.clone());

    let provider = crate::providers::resolver::resolve_provider(
        &provider_type,
        api_key,
        base_url,
        model.clone(),
        None,
    )
    .map_err(|e| {
        crate::error::SyscityError::Validation(format!(
            "Quality gate: failed to create provider '{}': {}",
            provider_type, e
        ))
    })?;

    // 2. Create eval tool registry
    let tool_registry = Arc::new(create_eval_tool_registry(None));

    // 3. Create a temporary agent for eval
    let agent_config = config.default_agent.clone();
    let agent = Arc::new(
        crate::agent::Agent::new(agent_config, provider.clone(), tool_registry.clone())
            .with_paths(state.paths.clone()),
    );

    // 4. Create critic (if needed for criteria)
    let mut critic = crate::agent::reflection::critic::Critic::new(provider);
    if let Some(ref model_name) = model {
        critic = critic.with_model(model_name.clone());
    }

    // 5. Build harness and gate.
    // The harness carries a LayeredScorer (§三 人工复核) so every trial routed
    // through the daemon gate persists a human-review case on low-confidence /
    // conflict signals, honoring `eval.human_review.sampling_rate`. The critic
    // is consumed by the harness; the scorer only calls `route_scored`, so it
    // is built with no internal critic.
    let evals_dir = crate::eval::loader::default_evals_dir();
    let harness = crate::eval::harness::EvalHarness::new(agent.clone(), Some(critic)).with_scorer(
        crate::eval::LayeredScorer::new(None, crate::eval::ScorerConfig::default())
            .with_review_store(crate::eval::HumanReviewStore::new(&evals_dir))
            .with_sampling_rate(config.eval.human_review.sampling_rate),
    );

    let gate = match crate::gateway::quality_gate::QualityGate::from_config(
        &config.quality_gate,
        harness,
        evals_dir,
        state.paths.clone(),
    ) {
        Some(g) => g,
        None => {
            info!("Quality gate not configured — skipping");
            return Ok(());
        }
    };
    let gate = gate.with_badcase_governance(crate::eval::recycle::BadcaseGovernance::from_config(
        &config.eval.badcase_governance,
    ));
    // Attach the runtime stores backing the compression gate and the shadow
    // gate's online replay (§十二 ⑧ / §09). `None` stores leave them inert.
    let gate = gate
        .with_stores(state.infra.pending_badcase_store.clone(), state.infra.sample_store.clone());

    // 6. Run the gate (returns result + release decision)
    let (result, decision) = gate.check().await;

    // 7. Print results
    let decision_label = match &decision {
        crate::gateway::quality_gate::ReleaseDecision::Proceed => "PROCEED",
        crate::gateway::quality_gate::ReleaseDecision::Rollback => "ROLLBACK",
        crate::gateway::quality_gate::ReleaseDecision::Degrade => "DEGRADE",
    };
    info!("{}", result);
    info!("Release decision: {}", decision_label);

    // 8. Cleanup
    agent.shutdown().await?;

    match decision {
        crate::gateway::quality_gate::ReleaseDecision::Proceed => {
            info!("✅ Quality gate passed — proceeding with startup");
            Ok(())
        }
        crate::gateway::quality_gate::ReleaseDecision::Rollback => {
            let err = crate::error::SyscityError::Validation(
                "Quality gate ROLLBACK — blocking startup".into(),
            );
            if config.quality_gate.shutdown_on_failure {
                Err(err)
            } else {
                warn!("{} (shutdown_on_failure disabled, continuing)", err);
                Ok(())
            }
        }
        crate::gateway::quality_gate::ReleaseDecision::Degrade => {
            warn!("⚠️ Quality gate DEGRADE — starting in degraded mode");
            Ok(())
        }
    }
}

/// Create a minimal tool registry for quality gate eval.
fn create_eval_tool_registry(
    acp: Option<Arc<crate::acp::AcpControlPlane>>,
) -> crate::tools::ToolRegistry {
    let mut registry = crate::tools::ToolRegistry::new();
    registry.register(Box::new(crate::tools::shell::ShellTool::new()));
    registry.register(Box::new(crate::tools::file::FileReadTool::new()));
    registry.register(Box::new(crate::tools::file::FileWriteTool::new()));
    registry.register(Box::new(crate::tools::file::FileEditTool::new()));
    registry.register(Box::new(crate::tools::grep::GrepTool::new()));
    registry.register(Box::new(crate::tools::file::GlobTool::new()));
    registry.register(Box::new(crate::tools::web::WebSearchTool::new()));
    registry.register(Box::new(crate::tools::web::WebFetchTool::new()));
    registry.register(Box::new(crate::tools::todo_tool::TodoTool::new()));
    registry.register(Box::new(crate::tools::time::TimeTool::new()));
    if let Some(acp) = acp {
        registry.register(Box::new(crate::tools::AcpSpawnTool::new(acp.clone(), None)));
        registry.register(Box::new(crate::tools::AcpSessionTool::new(acp.clone())));
        registry.register(Box::new(crate::tools::SessionsSendTool::new(acp)));
    }
    registry
}

/// Register the discovered tools of a connected MCP server into the agent
/// tool registry (`mcp__{server_id}__{tool}`). Shared by the boot-time
/// auto-connect and runtime add/connect paths so tools become available to
/// agents immediately.
pub(crate) async fn register_mcp_tools(
    state: &Arc<GatewayState>,
    server_id: &str,
    tools: &[crate::mcp::McpToolDefinition],
    max_tools_config: usize,
) {
    let max_tools = if max_tools_config == 0 {
        tools.len()
    } else {
        max_tools_config.min(tools.len())
    };

    if let Some(client_arc) = state.tools.mcp_manager.get_client(server_id).await {
        for tool in tools.iter().take(max_tools) {
            let wrapper = Arc::new(McpToolWrapper::new(client_arc.clone(), server_id, tool));
            state.tools.registry.register_dynamic(wrapper);
            debug!("  Registered MCP tool: mcp__{}__{}", server_id, tool.name);
        }
    }
}

/// Auto-connect MCP servers from config and register their tools.
///
/// Runs all connections concurrently in the background so they never block
/// the HTTP listener from starting.
pub(crate) fn init_mcp_servers(state: Arc<GatewayState>, config: &GatewayConfig) {
    let servers = &config.mcp.servers;
    if servers.is_empty() {
        debug!("No MCP servers configured");
        return;
    }

    info!("Auto-connecting {} configured MCP server(s) (background)…", servers.len());

    for (server_id, server_config) in servers {
        if !server_config.auto_connect {
            info!("MCP server '{}' has auto_connect=false, skipping", server_id);
            continue;
        }

        let bg_state = state.clone();
        let sid = server_id.clone();
        let cfg = server_config.clone();
        tokio::spawn(async move {
            match bg_state.tools.mcp_manager.connect(&sid, cfg.clone()).await {
                Ok(tools) => {
                    info!("✅ MCP server '{}' connected: {} tool(s) discovered", sid, tools.len());
                    register_mcp_tools(&bg_state, &sid, &tools, cfg.max_tools).await;
                }
                Err(e) => {
                    warn!("Failed to connect MCP server '{}': {}", sid, e);
                }
            }
        });
    }
}
