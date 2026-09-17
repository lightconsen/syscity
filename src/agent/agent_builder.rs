//! [`AgentBuilder`]: fluent construction of an [`Agent`].

use std::sync::Arc;

use crate::channels::thread_binding::ThreadBindingManager;
use crate::providers::Provider;
use crate::tools::ToolRegistry;

use super::*;

#[derive(Default)]
pub struct AgentBuilder {
    pub(super) config: Option<AgentConfig>,
    pub(super) provider: Option<Arc<dyn Provider>>,
    pub(super) tools: Option<Arc<ToolRegistry>>,
    session_search: Option<Arc<crate::memory::SessionSearch>>,
    transcript_store: Option<Arc<crate::agent::TranscriptStore>>,
    artifact_store: Option<Arc<crate::agent::ArtifactStore>>,
    disk_budget: Option<Arc<crate::agent::DiskBudgetManager>>,
    session_file_manager: Option<Arc<crate::agent::SessionFileManager>>,
    model_router: Option<Arc<crate::model_router::ModelRouter>>,
    /// Concrete model ID used for completions routed through the model router.
    model: Option<String>,
    /// Model name for the task planner (bypasses provider default).
    planner_model: Option<String>,
    skill_manager: Option<Arc<tokio::sync::RwLock<crate::skills::SkillManager>>>,
    /// PII detector for output content filtering.
    pii_detector: Option<Arc<crate::security::PiiDetector>>,
    /// Computer adapter for desktop automation.
    computer_adapter: Option<Arc<dyn crate::computer::ComputerAdapter>>,
    /// Configuration for the computer use loop.
    computer_config: Option<crate::computer::LoopConfig>,
    /// Thread binding manager for tracking session/thread hierarchy.
    thread_binding_manager: Option<ThreadBindingManager>,
}

impl AgentBuilder {
    /// Create a new builder
    pub fn new() -> Self {
        Self::default()
    }

    /// Set configuration
    pub fn config(mut self, config: AgentConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Set skills prompt
    pub fn skills(mut self, skills_prompt: String) -> Self {
        let mut config = self.config.unwrap_or_default();
        config.skills_prompt = Some(skills_prompt);
        self.config = Some(config);
        self
    }

    /// Set provider
    pub fn provider(mut self, provider: Arc<dyn Provider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Set tools
    pub fn tools(mut self, tools: Arc<ToolRegistry>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Set session search for conversation indexing
    pub fn session_search(mut self, search: Arc<crate::memory::SessionSearch>) -> Self {
        self.session_search = Some(search);
        self
    }

    /// Set transcript store for conversation recording
    pub fn transcript_store(mut self, store: Arc<crate::agent::TranscriptStore>) -> Self {
        self.transcript_store = Some(store);
        self
    }

    /// Set artifact store for session-bound artifacts
    pub fn artifact_store(mut self, store: Arc<crate::agent::ArtifactStore>) -> Self {
        self.artifact_store = Some(store);
        self
    }

    /// Set disk budget manager for per-session storage quota
    pub fn disk_budget(mut self, budget: Arc<crate::agent::DiskBudgetManager>) -> Self {
        self.disk_budget = Some(budget);
        self
    }

    /// Set session file manager for isolated per-session file ops
    pub fn session_file_manager(mut self, manager: Arc<crate::agent::SessionFileManager>) -> Self {
        self.session_file_manager = Some(manager);
        self
    }

    /// Set model router for advanced routing, key rotation, and fallback.
    pub fn model_router(mut self, router: Arc<crate::model_router::ModelRouter>) -> Self {
        self.model_router = Some(router);
        self
    }

    /// Set the concrete model ID used for completions routed through the model
    /// router.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Set the model name for the task planner's direct provider calls.
    /// This prevents the planner from using the provider's hardcoded default
    /// (e.g., gpt-4o-mini) which may not be supported by the actual API.
    pub fn planner_model(mut self, model: impl Into<String>) -> Self {
        self.planner_model = Some(model.into());
        self
    }

    /// Set reflection configuration for self-critique and iterative
    /// improvement.
    ///
    /// When enabled, the agent evaluates its own output via an LLM critic
    /// and iteratively improves responses that fall below quality thresholds.
    pub fn reflection_config(mut self, config: reflection::ReflectionConfig) -> Self {
        let mut cfg = self.config.unwrap_or_default();
        cfg.reflection_config = Some(config);
        self.config = Some(cfg);
        self
    }

    /// Set skill manager for dynamic skill injection.
    pub fn skill_manager(
        mut self,
        manager: Arc<tokio::sync::RwLock<crate::skills::SkillManager>>,
    ) -> Self {
        self.skill_manager = Some(manager);
        self
    }

    /// Set PII detector for output content filtering.
    pub fn with_pii_detector(mut self, detector: Arc<crate::security::PiiDetector>) -> Self {
        self.pii_detector = Some(detector);
        self
    }

    /// Set computer adapter for desktop automation.
    pub fn with_computer_adapter(
        mut self,
        adapter: Arc<dyn crate::computer::ComputerAdapter>,
    ) -> Self {
        self.computer_adapter = Some(adapter);
        self
    }

    /// Set configuration for the computer use loop.
    pub fn with_computer_config(mut self, config: crate::computer::LoopConfig) -> Self {
        self.computer_config = Some(config);
        self
    }

    /// Set thread binding manager for tracking session/thread hierarchy.
    pub fn with_thread_binding_manager(mut self, manager: ThreadBindingManager) -> Self {
        self.thread_binding_manager = Some(manager);
        self
    }

    /// Build the agent
    pub fn build(self) -> crate::Result<Agent> {
        let mut agent = Agent::new(
            self.config.unwrap_or_default(),
            self.provider.ok_or_else(|| {
                crate::error::SyscityError::Validation("Provider required".to_string())
            })?,
            self.tools.unwrap_or_else(|| Arc::new(ToolRegistry::new())),
        );

        if let Some(search) = self.session_search {
            agent = agent.with_session_search(search);
        }

        if let Some(store) = self.transcript_store {
            agent = agent.with_transcript_store(store);
        }

        if let Some(store) = self.artifact_store {
            agent = agent.with_artifact_store(store);
        }

        if let Some(budget) = self.disk_budget {
            agent = agent.with_disk_budget(budget);
        }

        if let Some(manager) = self.session_file_manager {
            agent = agent.with_session_file_manager(manager);
        }

        if let Some(router) = self.model_router {
            agent = agent.with_model_router(router);
        }

        if let Some(model) = self.model {
            agent = agent.with_model(model);
        }

        if let Some(model) = self.planner_model {
            // Update the task planner with the correct model name so it
            // doesn't fall back to the provider's hardcoded default
            let provider = agent.provider.clone();
            agent.task_planner =
                Arc::new(crate::agent::planner::TaskPlanner::new(provider).with_model(model));
        }

        if let Some(manager) = self.skill_manager {
            agent = agent.with_skill_manager(manager);
        }

        if let Some(detector) = self.pii_detector {
            agent = agent.with_pii_detector(detector);
        }

        if let Some(adapter) = self.computer_adapter {
            agent = agent.with_computer_adapter(adapter);
        }

        if let Some(config) = self.computer_config {
            agent = agent.with_computer_config(config);
        }

        if let Some(manager) = self.thread_binding_manager {
            agent = agent.with_thread_binding_manager(manager);
        }

        Ok(agent)
    }
}
