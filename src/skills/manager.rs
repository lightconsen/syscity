//! Skill manager: loading, hot reload, dependency resolution, and chaining.
//!
//! Implements [`SkillManager`] (struct defined in the parent module): skill
//! discovery, eligibility gating, hot reload, dependency graph resolution, and
//! execution-chain building.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};

use super::*;

impl SkillManager {
    /// Create a new skill manager rooted at `paths`.
    pub async fn new(paths: std::sync::Arc<crate::dirs::SyscityPaths>) -> crate::Result<Self> {
        let storage = SkillStorage::new(paths.clone())?;
        let config = match SkillConfig::load().await {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to load skill config (using defaults): {}", e);
                SkillConfig::default()
            }
        };
        let (reload_tx, reload_rx) = mpsc::channel(100);

        let manager = Self {
            storage,
            skills: Arc::new(RwLock::new(HashMap::new())),
            config,
            watcher: None,
            reload_tx,
            reload_rx: Arc::new(RwLock::new(reload_rx)),
            paths,
        };

        Ok(manager)
    }

    /// Initialize and load all skills
    pub async fn initialize(&mut self) -> crate::Result<usize> {
        // Load skills from all storage locations
        let count = self.load_all().await?;

        // Validate dependency graph and version constraints
        let graph = self.build_dependency_graph().await;
        match graph.check_versions() {
            Ok(()) => info!("Skill dependency version checks passed"),
            Err(e) => warn!("Skill dependency version issue: {}", e),
        }

        // Resolve all skills in dependency order
        match self.resolve_all_dependencies().await {
            Ok(order) => {
                info!("Skills loaded in dependency order: {}", order.join(", "));
            }
            Err(e) => {
                warn!("Skill dependency resolution failed (startup continues): {}", e);
            }
        }

        // Start file watcher for hot reloading
        self.start_watcher().await?;

        // Start reload processor
        self.start_reload_processor();

        info!("Skill manager initialized with {} skills", count);
        Ok(count)
    }

    /// Load all skills from all storage locations
    pub async fn load_all(&self) -> crate::Result<usize> {
        let mut total_count = 0;

        let mut skills = self.skills.write().await;
        skills.clear();

        // First, load built-in skills (lowest priority, can be overridden)
        let builtin_skills = builtin::get_builtin_skills();
        for (name, skill) in builtin_skills {
            info!(
                "Loaded built-in skill: {} (eligible: {}, enabled: {})",
                name, skill.is_eligible, skill.enabled
            );
            skills.insert(name, skill);
            total_count += 1;
        }

        // Then load skills from storage (user, workspace, project)
        let skill_files = self.storage.discover_all().await;

        for skill_location in skill_files {
            let path = &skill_location.skill_file;
            match Self::load_skill_from_file_inner(path).await {
                Ok(mut skill) => {
                    // Check eligibility
                    skill.check_eligibility();

                    // Check if skill is enabled in config
                    skill.enabled = self
                        .config
                        .entries
                        .get(&skill.name)
                        .map(|e| e.enabled)
                        .unwrap_or(true);

                    // Set source level from discovery
                    skill.source_level = skill_location.level;

                    // Check if this is overriding a built-in skill
                    let is_override = skills.contains_key(&skill.name);
                    if is_override {
                        info!(
                            "Overriding built-in skill: {} with version from {:?}",
                            skill.name, skill_location.level
                        );
                    }

                    info!(
                        "Loaded skill: {} (eligible: {}, enabled: {}, level: {:?})",
                        skill.name, skill.is_eligible, skill.enabled, skill.source_level
                    );
                    skills.insert(skill.name.clone(), skill);
                    total_count += 1;
                }
                Err(e) => {
                    warn!("Failed to load skill from {:?}: {}", path, e);
                }
            }
        }

        Ok(total_count)
    }

    /// Start file watcher for hot reloading
    async fn start_watcher(&mut self) -> crate::Result<()> {
        let _skills = Arc::clone(&self.skills);
        let reload_tx = self.reload_tx.clone();
        let storage_paths = self.storage.get_all_paths();

        let watcher = SkillWatcher::new(storage_paths, move |path| {
            if let Err(e) = reload_tx.try_send(path) {
                warn!("Hot-reload channel full, reload event dropped: {}", e);
            }
        })?;

        self.watcher = Some(watcher);
        info!("Started skill file watcher");

        Ok(())
    }

    /// Start background task to process reloads
    fn start_reload_processor(&self) {
        let skills = Arc::clone(&self.skills);
        let reload_rx = Arc::clone(&self.reload_rx);

        tokio::spawn(async move {
            let mut rx = reload_rx.write().await;
            while let Some(path) = rx.recv().await {
                info!("Hot reloading skill from: {}", path);

                // Try to reload the skill
                if let Err(e) = Self::reload_skill(&skills, &path).await {
                    error!("Failed to reload skill from {}: {}", path, e);
                }
            }
        });
    }

    /// Reload a single skill
    async fn reload_skill(
        skills: &Arc<RwLock<HashMap<String, Skill>>>,
        path: &str,
    ) -> crate::Result<()> {
        let path = Path::new(path);

        // Load the skill
        let content = tokio::fs::read_to_string(path).await?;
        let (frontmatter, prompt) = frontmatter::parse_skill_md(&content)?;

        let mut skill: Skill = serde_norway::from_str(&frontmatter)?;
        skill.prompt = prompt;
        skill.source_path = path.to_path_buf();
        skill.check_eligibility();

        // Update in memory
        let mut skills_guard = skills.write().await;
        skills_guard.insert(skill.name.clone(), skill);

        info!("Hot reloaded skill: {}", path.display());
        Ok(())
    }

    /// Get a skill by name
    pub async fn get_skill(&self, name: &str) -> Option<Skill> {
        let skills = self.skills.read().await;
        skills.get(name).cloned()
    }

    /// Insert a skill directly, bypassing guard/validation — test-only.
    #[cfg(test)]
    pub(crate) async fn insert_for_test(&self, skill: Skill) {
        self.skills.write().await.insert(skill.name.clone(), skill);
    }

    /// Activate a skill with runtime requirement verification.
    ///
    /// Unlike `get_skill()` which returns the cached skill,
    /// this verifies all `requires` fields are still met at activation time.
    pub async fn activate_skill(&self, name: &str) -> crate::Result<Skill> {
        let skill =
            self.get_skill(name)
                .await
                .ok_or_else(|| crate::error::SyscityError::NotFound {
                    resource: format!("Skill: {}", name),
                })?;

        // Runtime verification - re-check requirements at activation
        match skill.verify_requirements() {
            Ok(()) => Ok(skill),
            Err(errors) => {
                warn!("Skill '{}' activation blocked: requirements not met: {:?}", name, errors);
                Err(crate::error::SyscityError::Validation(format!(
                    "Skill '{}' requirements not met: {}",
                    name,
                    errors.join(", ")
                )))
            }
        }
    }

    /// List all loaded skills
    pub async fn list_skills(&self) -> Vec<Skill> {
        let skills = self.skills.read().await;
        skills.values().cloned().collect()
    }

    /// Get the maximum number of skills to include in a prompt.
    pub fn max_skills_in_prompt(&self) -> usize {
        self.config.limits.max_skills_in_prompt
    }

    /// Get the maximum total characters for the skills prompt section.
    pub fn max_skills_prompt_chars(&self) -> usize {
        self.config.limits.max_skills_prompt_chars
    }

    /// List eligible skills only
    pub async fn list_eligible_skills(&self) -> Vec<Skill> {
        let skills = self.skills.read().await;
        skills.values().filter(|s| s.is_eligible).cloned().collect()
    }

    /// Find skills matching user input
    pub async fn find_matching_skills(&self, input: &str) -> Vec<Skill> {
        let skills = self.skills.read().await;
        skills
            .values()
            .filter(|s| s.is_eligible && s.matches(input))
            .cloned()
            .collect()
    }

    /// Deterministic skill prefilter (no LLM call).
    ///
    /// Runs keyword / regex matching against eligible skills and returns at
    /// most `max_skills` results. Results are ordered by trust level
    /// (highest first) so that `Trusted` skills are always preferred over
    /// `Community` skills when the cap is reached. This prevents prompt
    /// injection through an unbounded number of community-skill system
    /// prompts being injected into the agent context.
    ///
    /// Pass `max_skills = 0` to disable the count cap.
    ///
    /// When `max_prompt_chars > 0`, the total combined prompt text of the
    /// returned skills is pruned (lowest-trust skills removed first) until
    /// it fits within the character budget. This is the token-optimisation
    /// pass.
    pub async fn prefilter_skills(
        &self,
        input: &str,
        max_skills: usize,
        max_prompt_chars: usize,
    ) -> Vec<Skill> {
        let skills = self.skills.read().await;
        let mut matched: Vec<Skill> = skills
            .values()
            .filter(|s| s.is_eligible && s.matches(input))
            .cloned()
            .collect();

        // Prefer higher-trust skills first.
        matched.sort_by_key(|b| std::cmp::Reverse(b.metadata.trust));

        // Store trigger text for prompt annotation.
        for skill in &mut matched {
            skill.trigger_text = skill.find_trigger_text(input);
        }

        if max_skills > 0 {
            matched.truncate(max_skills);
        }

        // Prune by total prompt character budget (token optimisation).
        // Remove lowest-trust skills first until total fits.
        if max_prompt_chars > 0 {
            let mut total_chars: usize = matched
                .iter()
                .map(|s| s.to_prompt_section(None).len())
                .sum();
            while total_chars > max_prompt_chars && matched.len() > 1 {
                // Remove the last (lowest-trust) skill.
                if let Some(removed) = matched.pop() {
                    total_chars = total_chars.saturating_sub(removed.to_prompt_section(None).len());
                }
            }
            if total_chars > max_prompt_chars && !matched.is_empty() {
                warn!(
                    "Skills prompt ({} chars) still exceeds budget ({} chars) after pruning to {} \
                     skill(s)",
                    total_chars,
                    max_prompt_chars,
                    matched.len()
                );
            }
        }

        matched
    }

    /// Compute the minimum trust level across a slice of skills.
    ///
    /// The result constrains the tool set: if any active skill is
    /// `Community`-trust the agent must restrict itself to non-privileged
    /// tools.
    pub fn min_trust(skills: &[Skill]) -> crate::tools::SkillTrust {
        skills
            .iter()
            .map(|s| s.metadata.trust)
            .min()
            .unwrap_or(crate::tools::SkillTrust::Trusted)
    }

    /// Get skills as formatted prompt text
    pub async fn build_skills_prompt(&self, compact: bool) -> String {
        let skills = self.list_eligible_skills().await;

        if skills.is_empty() {
            return "No skills available.".to_string();
        }

        let mut output = format!("Available Skills ({}):\n\n", skills.len());

        for skill in skills {
            output.push_str(&skill.format_for_prompt(compact));
            output.push('\n');
        }

        output
    }

    /// Render the stable name+description catalog of all eligible skills.
    ///
    /// The catalog contains no skill bodies — the model loads them on demand
    /// via the `skill` tool. It is identical across messages (changing only
    /// on skill add/remove/reload), which keeps the system prompt prefix
    /// stable and provider prompt caches effective. Returns an empty string
    /// when no eligible skills exist.
    pub async fn build_catalog(&self) -> String {
        let mut skills = self.list_eligible_skills().await;
        if skills.is_empty() {
            return String::new();
        }

        // Deterministic order: trust first, then name — the catalog must be
        // byte-identical across messages.
        skills.sort_by(|a, b| {
            std::cmp::Reverse(a.metadata.trust)
                .cmp(&std::cmp::Reverse(b.metadata.trust))
                .then_with(|| a.name.cmp(&b.name))
        });
        let max = self.max_skills_in_prompt();
        if max > 0 {
            skills.truncate(max);
        }

        let mut output = String::from("## Available Skills\n\n");
        for skill in &skills {
            output.push_str(skill.format_for_prompt(false).trim_end());
            output.push('\n');
        }
        output.push_str("\nCall the `skill` tool with a skill name to load its full instructions.");
        output
    }

    /// Create a new skill
    pub async fn create_skill(&self, skill: &Skill) -> crate::Result<()> {
        // Check security
        let report = guard::scan_skill(skill);
        if !report.passed {
            return Err(crate::error::SyscityError::Validation(format!(
                "Security check failed: {:?}",
                report.issues
            )));
        }

        // Validate
        if let Err(errors) = guard::validate_skill(skill) {
            return Err(crate::error::SyscityError::Validation(errors.join(", ")));
        }

        // Write to user skills directory
        let user_dir = self.storage.user_dir();
        let skill_dir = user_dir.join(&skill.name);
        tokio::fs::create_dir_all(&skill_dir).await?;

        let skill_file = skill_dir.join("SKILL.md");

        // Format as SKILL.md
        let emoji = skill.metadata.emoji.clone();
        let content =
            frontmatter::format_skill_md(&skill.name, &skill.description, &skill.prompt, &emoji);
        tokio::fs::write(&skill_file, content).await?;

        info!("Created skill: {} at {:?}", skill.name, skill_file);
        Ok(())
    }

    /// Delete a skill
    pub async fn delete_skill(&mut self, name: &str) -> crate::Result<bool> {
        let skill_dir = self.storage.user_dir().join(name);

        if skill_dir.exists() {
            tokio::fs::remove_dir_all(&skill_dir).await?;

            let mut skills = self.skills.write().await;
            skills.remove(name);

            info!("Deleted skill: {}", name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Public reload: re-scan all skill directories and update in-memory map.
    ///
    /// Acquires a write lock on `self.skills`, clears the map, and
    /// re-discovers all skills from every storage level (built-in, user,
    /// workspace, project). This lets daemon processes pick up
    /// registry-downloaded or locally-installed skills without a restart.
    ///
    /// Delegates to [`load_all`] which contains the canonical loading logic.
    pub async fn reload(&self) -> crate::Result<usize> {
        info!("Reloading all skills from storage");
        let count = self.load_all().await?;
        info!("Skill reload complete: {} skills loaded", count);
        Ok(count)
    }

    /// Load a skill from file (static helper for reload).
    async fn load_skill_from_file_inner(path: &Path) -> crate::Result<Skill> {
        let content = tokio::fs::read_to_string(path).await?;
        let (frontmatter, prompt) = frontmatter::parse_skill_md(&content)?;
        let mut skill: Skill = serde_norway::from_str(&frontmatter)?;
        skill.prompt = prompt;
        skill.source_path = path.to_path_buf();
        let file_size = content.len();
        if file_size > skill.metadata.max_size {
            return Err(crate::error::SyscityError::Validation(format!(
                "Skill file too large: {} bytes (max: {})",
                file_size, skill.metadata.max_size
            )));
        }
        Ok(skill)
    }

    /// Install a skill from the remote registry and reload.
    ///
    /// Uses `SkillRegistry` to download the skill into
    /// `~/.syscity/skills/{name}/`, then calls `reload()` so the new skill
    /// is picked up without a restart.
    pub async fn install_from_registry(
        &self,
        name: &str,
        registry_url: Option<&str>,
    ) -> crate::Result<()> {
        let registry = match registry_url {
            Some(url) => registry::SkillRegistry::new(url, self.paths.clone())?,
            None => registry::SkillRegistry::default_registry(self.paths.clone())?,
        };

        info!("Installing skill '{}' from registry", name);
        registry.install(name).await?;

        // Reload to pick up the newly installed skill
        self.reload().await?;

        info!("Skill '{}' installed and loaded", name);
        Ok(())
    }

    /// Uninstall a skill and reload.
    ///
    /// Removes `~/.syscity/skills/{name}/`, then calls `reload()` to
    /// remove it from the in-memory map.
    pub async fn uninstall_skill(&self, name: &str) -> crate::Result<bool> {
        let skill_dir = self.storage.user_dir().join(name);

        if skill_dir.exists() {
            tokio::fs::remove_dir_all(&skill_dir).await?;

            // Reload to update in-memory map
            self.reload().await?;

            info!("Uninstalled skill: {}", name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Install a skill's dependencies
    pub async fn install_skill(&self, name: &str) -> crate::Result<Vec<InstallResult>> {
        let skill =
            self.get_skill(name)
                .await
                .ok_or_else(|| crate::error::SyscityError::NotFound {
                    resource: format!("Skill: {}", name),
                })?;

        let mut results = Vec::new();

        for spec in &skill.metadata.install {
            match install::install_skill(spec).await {
                Ok(result) => results.push(result),
                Err(e) => {
                    error!("Failed to install {:?}: {}", spec, e);
                    results.push(InstallResult::Failed {
                        spec: spec.clone(),
                        error: e.to_string(),
                    });
                }
            }
        }

        Ok(results)
    }

    /// Enable/disable a skill in config
    pub async fn set_skill_enabled(&mut self, name: &str, enabled: bool) -> crate::Result<()> {
        let entry = self.config.entries.entry(name.to_string()).or_default();
        entry.enabled = enabled;
        self.config.save().await?;

        // Update in-memory skill if present
        let mut skills = self.skills.write().await;
        if let Some(_skill) = skills.get_mut(name) {
            // Note: skill eligibility is separate from config enabled state
            info!("Skill {} enabled state changed to: {}", name, enabled);
        }

        Ok(())
    }

    // ------------------------------------------------------------------
    // Dependency resolution
    // ------------------------------------------------------------------

    /// Build a dependency graph from all loaded skills
    pub async fn build_dependency_graph(&self) -> dependencies::DependencyGraph {
        let skills = self.skills.read().await;
        let mut graph = dependencies::DependencyGraph::new();

        for skill in skills.values() {
            let version = match semver::Version::parse(&skill.version) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Skill '{}' has invalid version '{}': {}", skill.name, skill.version, e);
                    continue;
                }
            };

            let deps: Vec<_> = skill
                .depends_on
                .iter()
                .filter_map(|(dep_name, dep_constraint)| {
                    let spec = format!("{}: {}", dep_name, dep_constraint);
                    dependencies::DependencySpec::parse(&spec)
                        .map_err(|e| {
                            warn!(
                                "Invalid dependency spec '{}' for skill '{}': {}",
                                spec, skill.name, e
                            );
                        })
                        .ok()
                })
                .collect();

            let provides = skill.provides.clone();

            graph.add_node(dependencies::DependencyNode {
                name: skill.name.clone(),
                version,
                dependencies: deps,
                provides,
            });
        }

        graph
    }

    /// Resolve dependencies for a skill and return activation order
    pub async fn resolve_dependencies(&self, name: &str) -> crate::Result<Vec<String>> {
        let graph = self.build_dependency_graph().await;

        match graph.resolve(name) {
            Ok(order) => {
                info!("Resolved {} dependencies for '{}'", order.len(), name);
                Ok(order)
            }
            Err(e) => {
                error!("Dependency resolution failed for '{}': {}", name, e);
                Err(crate::error::SyscityError::Validation(format!(
                    "Dependency resolution failed: {}",
                    e
                )))
            }
        }
    }

    /// Resolve all loaded skills in dependency order
    pub async fn resolve_all_dependencies(&self) -> crate::Result<Vec<String>> {
        let graph = self.build_dependency_graph().await;

        match graph.check_versions() {
            Ok(()) => {}
            Err(e) => {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Version check failed: {}",
                    e
                )));
            }
        }

        match graph.resolve_all() {
            Ok(order) => {
                info!("Resolved {} skills in dependency order", order.len());
                Ok(order)
            }
            Err(e) => {
                error!("Dependency resolution failed: {}", e);
                Err(crate::error::SyscityError::Validation(format!(
                    "Dependency resolution failed: {}",
                    e
                )))
            }
        }
    }

    /// Install all dependencies for a skill (both binary deps and skill deps)
    pub async fn install_all_dependencies(&self, name: &str) -> crate::Result<Vec<InstallResult>> {
        let mut results = Vec::new();

        // First install binary dependencies
        let binary_results = self.install_skill(name).await?;
        results.extend(binary_results);

        // Then resolve and install skill dependencies
        let order = self.resolve_dependencies(name).await?;
        for dep_name in order {
            if dep_name != name {
                if let Some(dep_skill) = self.get_skill(&dep_name).await {
                    for spec in &dep_skill.metadata.install {
                        match install::install_skill(spec).await {
                            Ok(result) => results.push(result),
                            Err(e) => {
                                warn!("Failed to install dependency for '{}': {}", dep_name, e);
                            }
                        }
                    }
                }
            }
        }

        Ok(results)
    }

    // ------------------------------------------------------------------
    // Skill chaining
    // ------------------------------------------------------------------

    /// Build an execution chain for a skill
    /// Returns the ordered list of skills to execute (including dependencies)
    pub async fn build_execution_chain(&self, name: &str) -> crate::Result<SkillChain> {
        let skills = self.skills.read().await;

        let root_skill =
            skills
                .get(name)
                .cloned()
                .ok_or_else(|| crate::error::SyscityError::NotFound {
                    resource: format!("Skill: {}", name),
                })?;

        let mut chain = Vec::new();
        let mut visited = std::collections::HashSet::new();

        // First add dependencies in order
        drop(skills);
        let deps = self.resolve_dependencies(name).await?;
        for dep_name in deps {
            if dep_name != name && visited.insert(dep_name.clone()) {
                if let Some(skill) = self.get_skill(&dep_name).await {
                    chain.push(skill);
                }
            }
        }

        // Then add the root skill
        if visited.insert(name.to_string()) {
            chain.push(root_skill.clone());
        }

        // Add chained skills (skills that follow the root in the pipeline)
        let skills = self.skills.read().await;
        for chained_name in &root_skill.chain {
            if visited.insert(chained_name.clone()) {
                if let Some(skill) = skills.get(chained_name) {
                    chain.push(skill.clone());
                }
            }
        }

        Ok(SkillChain {
            skills: chain,
            trigger_skill: name.to_string(),
        })
    }

    /// Execute a chain of skills, returning the combined prompt
    pub async fn execute_chain(&self, name: &str, _input: &str) -> crate::Result<String> {
        let chain = self.build_execution_chain(name).await?;

        let mut combined_prompt = String::new();
        combined_prompt.push_str(&format!("# Skill Chain: {}\n\n", chain.trigger_skill));

        for (i, skill) in chain.skills.iter().enumerate() {
            combined_prompt.push_str(&format!("## Step {}: {}\n\n", i + 1, skill.name));
            combined_prompt.push_str(&skill.to_prompt_section(None));
            combined_prompt.push_str("\n\n---\n\n");
        }

        Ok(combined_prompt)
    }

    /// Check if all dependencies for a skill are satisfied
    pub async fn check_dependencies(&self, name: &str) -> DependencyCheckResult {
        let graph = self.build_dependency_graph().await;

        let mut missing = Vec::new();
        let mut version_mismatches = Vec::new();

        if let Some(node) = graph.get(name) {
            for dep in &node.dependencies {
                if let Some(dep_node) = graph.get(&dep.name) {
                    if !dep.is_satisfied_by(&dep_node.version) {
                        version_mismatches.push(VersionMismatch {
                            skill: dep.name.clone(),
                            required: dep.version_req.to_string(),
                            found: dep_node.version.to_string(),
                        });
                    }
                } else {
                    missing.push(dep.name.clone());
                }
            }
        }

        let satisfied = missing.is_empty() && version_mismatches.is_empty();

        DependencyCheckResult {
            satisfied,
            missing,
            version_mismatches,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_min_trust_empty() {
        let skills: &[Skill] = &[];
        assert_eq!(SkillManager::min_trust(skills), crate::tools::SkillTrust::Trusted);
    }

    #[tokio::test]
    async fn test_skill_manager_dependency_graph_empty() {
        let manager = SkillManager::new(crate::dirs::paths()).await.unwrap();
        let graph = manager.build_dependency_graph().await;
        assert!(graph.names().is_empty());
    }

    #[tokio::test]
    async fn test_skill_manager_dependency_graph_with_skills() {
        let manager = SkillManager::new(crate::dirs::paths()).await.unwrap();

        // Insert a skill with dependencies directly
        {
            let mut skills = manager.skills.write().await;
            let mut base = Skill::new("base", "Base", "Base prompt");
            base.version = "1.0.0".to_string();
            skills.insert("base".to_string(), base);

            let mut app = Skill::new("app", "App", "App prompt");
            app.version = "1.0.0".to_string();
            app.depends_on
                .insert("base".to_string(), ">=1.0.0".to_string());
            skills.insert("app".to_string(), app);
        }

        let graph = manager.build_dependency_graph().await;
        assert!(graph.has("base"));
        assert!(graph.has("app"));
    }

    #[tokio::test]
    async fn test_skill_manager_resolve_dependencies() {
        let manager = SkillManager::new(crate::dirs::paths()).await.unwrap();

        {
            let mut skills = manager.skills.write().await;
            let mut base = Skill::new("base", "Base", "Base prompt");
            base.version = "1.0.0".to_string();
            skills.insert("base".to_string(), base);

            let mut app = Skill::new("app", "App", "App prompt");
            app.version = "1.0.0".to_string();
            app.depends_on
                .insert("base".to_string(), ">=1.0.0".to_string());
            skills.insert("app".to_string(), app);
        }

        let order = manager.resolve_dependencies("app").await.unwrap();
        assert_eq!(order, vec!["base", "app"]);
    }

    #[tokio::test]
    async fn test_build_catalog_format_and_stability() {
        let manager = SkillManager::new(crate::dirs::paths()).await.unwrap();
        {
            let mut skills = manager.skills.write().await;
            skills.insert(
                "alpha".to_string(),
                Skill::new("alpha", "Alpha does X", "FULL_BODY_ALPHA"),
            );
            skills.insert("beta".to_string(), Skill::new("beta", "Beta does Y", "FULL_BODY_BETA"));
            let mut hidden = Skill::new("hidden", "Hidden", "HIDDEN_BODY");
            hidden.is_eligible = false;
            skills.insert("hidden".to_string(), hidden);
        }

        let catalog = manager.build_catalog().await;
        assert!(catalog.starts_with("## Available Skills"));
        assert!(catalog.contains("**alpha**"));
        assert!(catalog.contains("Alpha does X"));
        assert!(catalog.contains("**beta**"));
        assert!(!catalog.contains("hidden"), "ineligible skills excluded");
        assert!(!catalog.contains("FULL_BODY"), "catalog carries no bodies");
        assert!(catalog.contains("`skill` tool"), "usage note present");
        // Byte-identical across calls — prompt prefix stability.
        assert_eq!(catalog, manager.build_catalog().await);
    }

    #[tokio::test]
    async fn test_build_catalog_empty() {
        let manager = SkillManager::new(crate::dirs::paths()).await.unwrap();
        assert_eq!(manager.build_catalog().await, "");
    }

    #[tokio::test]
    async fn test_skill_manager_check_dependencies_satisfied() {
        let manager = SkillManager::new(crate::dirs::paths()).await.unwrap();

        {
            let mut skills = manager.skills.write().await;
            let mut base = Skill::new("base", "Base", "Base prompt");
            base.version = "1.0.0".to_string();
            skills.insert("base".to_string(), base);

            let mut app = Skill::new("app", "App", "App prompt");
            app.version = "1.0.0".to_string();
            app.depends_on
                .insert("base".to_string(), ">=1.0.0".to_string());
            skills.insert("app".to_string(), app);
        }

        let check = manager.check_dependencies("app").await;
        assert!(check.satisfied);
        assert!(check.missing.is_empty());
    }

    #[tokio::test]
    async fn test_skill_manager_check_dependencies_missing() {
        let manager = SkillManager::new(crate::dirs::paths()).await.unwrap();

        {
            let mut skills = manager.skills.write().await;
            let mut app = Skill::new("app", "App", "App prompt");
            app.version = "1.0.0".to_string();
            app.depends_on
                .insert("missing".to_string(), ">=1.0.0".to_string());
            skills.insert("app".to_string(), app);
        }

        let check = manager.check_dependencies("app").await;
        assert!(!check.satisfied);
        assert_eq!(check.missing, vec!["missing"]);
    }
}
