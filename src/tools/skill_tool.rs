//! On-demand skill body loader: the `skill` tool.
//!
//! The system prompt carries only the name+description catalog of skills
//! (see `SkillManager::build_catalog`); the model calls this tool to pull a
//! skill's full instructions — plus its dependencies, in dependency order —
//! when it actually needs them.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::RwLock;

use super::{create_schema, Tool, ToolContext, ToolExecutionResult};
use crate::skills::SkillManager;

/// Loads a skill's full instructions by name.
pub struct SkillTool {
    manager: Arc<RwLock<SkillManager>>,
}

impl SkillTool {
    /// Create a new skill tool backed by the shared skill manager.
    pub fn new(manager: Arc<RwLock<SkillManager>>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Load a skill's full instructions by name. The system prompt lists available skills; \
         call this with a skill name to get its complete instructions before following them."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Load a skill's full instructions",
            serde_json::json!({
                "name": {
                    "type": "string",
                    "description": "Skill name from the Available Skills catalog"
                }
            }),
            vec!["name"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let name = args["name"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("Missing 'name' argument".to_string())
        })?;

        let manager = self.manager.read().await;

        if manager.get_skill(name).await.is_none() {
            let available: Vec<String> = manager
                .list_eligible_skills()
                .await
                .iter()
                .map(|s| s.name.clone())
                .collect();
            return Ok(ToolExecutionResult::error(format!(
                "Unknown skill: '{}'. Available skills: {}",
                name,
                available.join(", ")
            )));
        }

        // Dependency order puts prerequisites first, the requested skill last.
        let order = match manager.resolve_dependencies(name).await {
            Ok(order) => order,
            Err(e) => {
                return Ok(ToolExecutionResult::error(format!(
                    "Skill '{}' dependencies could not be resolved: {}",
                    name, e
                )))
            }
        };

        let mut sections = Vec::new();
        for skill_name in &order {
            // activate_skill re-verifies runtime requirements (bins, env, os).
            match manager.activate_skill(skill_name).await {
                Ok(skill) => sections.push(skill.to_prompt_section(None)),
                Err(e) => {
                    return Ok(ToolExecutionResult::error(format!(
                        "Skill '{}' is unavailable: {}",
                        skill_name, e
                    )))
                }
            }
        }

        Ok(ToolExecutionResult::success(sections.join("\n\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::Skill;

    async fn manager_with_skills() -> Arc<RwLock<SkillManager>> {
        let manager = SkillManager::new(crate::dirs::paths()).await.unwrap();
        let mut base = Skill::new("base", "Base skill", "BASE_BODY");
        base.version = "1.0.0".to_string();
        manager.insert_for_test(base).await;

        let mut app = Skill::new("app", "App skill", "APP_BODY");
        app.version = "1.0.0".to_string();
        app.depends_on
            .insert("base".to_string(), ">=1.0.0".to_string());
        manager.insert_for_test(app).await;

        manager
            .insert_for_test(Skill::new("solo", "Solo skill", "SOLO_BODY"))
            .await;
        Arc::new(RwLock::new(manager))
    }

    #[tokio::test]
    async fn test_loads_skill_body_by_name() {
        let tool = SkillTool::new(manager_with_skills().await);
        let ctx = ToolContext::new("user", "conv1");
        let result = tool
            .execute(serde_json::json!({"name": "solo"}), &ctx)
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("SOLO_BODY"));
        assert!(!result.output.contains("BASE_BODY"));
    }

    #[tokio::test]
    async fn test_dependencies_prepended_in_order() {
        let tool = SkillTool::new(manager_with_skills().await);
        let ctx = ToolContext::new("user", "conv1");
        let result = tool
            .execute(serde_json::json!({"name": "app"}), &ctx)
            .await
            .unwrap();
        assert!(result.success);
        let base_pos = result.output.find("BASE_BODY").expect("dep body present");
        let app_pos = result.output.find("APP_BODY").expect("skill body present");
        assert!(base_pos < app_pos, "dependency body must precede the skill body");
    }

    #[tokio::test]
    async fn test_unknown_skill_lists_available() {
        let tool = SkillTool::new(manager_with_skills().await);
        let ctx = ToolContext::new("user", "conv1");
        let result = tool
            .execute(serde_json::json!({"name": "ghost"}), &ctx)
            .await
            .unwrap();
        assert!(!result.success);
        let error = result.error.unwrap();
        assert!(error.contains("Unknown skill: 'ghost'"));
        assert!(error.contains("solo") && error.contains("app"));
    }

    #[tokio::test]
    async fn test_missing_name_argument() {
        let tool = SkillTool::new(manager_with_skills().await);
        let ctx = ToolContext::new("user", "conv1");
        let result = tool.execute(serde_json::json!({}), &ctx).await;
        assert!(result.is_err());
    }
}
