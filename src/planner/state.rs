//! Persistent task state store for the planner.
//!
//! Saves plan and task execution state to SQLite so that long-running goals
//! survive process restarts and can be resumed.

use std::time::Duration;

use serde_json;
use sqlx::{sqlite::SqlitePoolOptions, Pool, Row, Sqlite};
use tracing::{info, instrument};

use crate::planner::{Plan, Task, TaskStatus};

/// SQLite-backed persistent store for planner state.
#[derive(Debug, Clone)]
pub struct TaskStateStore {
    pool: Pool<Sqlite>,
}

impl TaskStateStore {
    /// Create a new state store at the given database URL.
    ///
    /// Example: `sqlite://~/.syscity/planner.db`
    pub async fn new(database_url: &str) -> crate::Result<Self> {
        info!("Initializing planner state store");

        if database_url.starts_with("sqlite://") && !database_url.contains(":memory:") {
            let path_str = database_url
                .strip_prefix("sqlite://")
                .unwrap_or(database_url);
            let path = std::path::Path::new(path_str);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    crate::error::SyscityError::Storage {
                        context: format!("Failed to create planner state directory: {:?}", parent),
                        details: e.to_string(),
                    }
                })?;
            }
            // sqlx 0.8 defaults `create_if_missing` to false; explicitly create
            // the file so a fresh install can open the database (mirrors
            // `gateway/init/storage.rs`).
            if !path.exists() {
                tokio::fs::File::create(path).await.map_err(|e| {
                    crate::error::SyscityError::Storage {
                        context: format!("Failed to create planner state file: {:?}", path),
                        details: e.to_string(),
                    }
                })?;
            }
        }

        let pool = SqlitePoolOptions::new()
            .max_connections(3)
            .acquire_timeout(Duration::from_secs(30))
            .connect(database_url)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to connect to planner state database".to_string(),
                details: e.to_string(),
            })?;

        let store = Self { pool };
        store.init_schema().await?;
        Ok(store)
    }

    async fn init_schema(&self) -> crate::Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS planner_plans (
                id TEXT PRIMARY KEY,
                goal TEXT NOT NULL,
                created_at TEXT NOT NULL,
                completed_at TEXT,
                success INTEGER
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to create planner_plans table".to_string(),
            details: e.to_string(),
        })?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS planner_tasks (
                id TEXT NOT NULL,
                plan_id TEXT NOT NULL,
                description TEXT NOT NULL,
                dependencies TEXT NOT NULL DEFAULT '[]',
                action_json TEXT,
                verification_json TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                result TEXT,
                error TEXT,
                max_retries INTEGER NOT NULL DEFAULT 2,
                retry_delay_ms INTEGER NOT NULL DEFAULT 1000,
                snapshot_before INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (plan_id, id),
                FOREIGN KEY (plan_id) REFERENCES planner_plans(id) ON DELETE CASCADE
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to create planner_tasks table".to_string(),
            details: e.to_string(),
        })?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_planner_tasks_plan ON planner_tasks(plan_id)")
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to create planner_tasks index".to_string(),
                details: e.to_string(),
            })?;

        Ok(())
    }

    /// Save a plan and all its tasks to the database.
    #[instrument(skip(self, plan))]
    pub async fn save_plan(&self, plan_id: &str, plan: &Plan) -> crate::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();

        sqlx::query(
            r#"
            INSERT INTO planner_plans (id, goal, created_at)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(id) DO UPDATE SET goal = excluded.goal
            "#,
        )
        .bind(plan_id)
        .bind(&plan.goal)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to save plan".to_string(),
            details: e.to_string(),
        })?;

        for task in plan.tasks.values() {
            self.save_task(plan_id, task).await?;
        }

        Ok(())
    }

    /// Save or update a single task.
    #[instrument(skip(self, task))]
    pub async fn save_task(&self, plan_id: &str, task: &Task) -> crate::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let deps = serde_json::to_string(&task.dependencies).unwrap_or_else(|_| "[]".to_string());
        let action_json = serde_json::to_string(&task.action).ok();
        let verification_json = task
            .verification
            .as_ref()
            .and_then(|v| serde_json::to_string(v).ok());
        let status_str = format!("{:?}", task.status);

        sqlx::query(
            r#"
            INSERT INTO planner_tasks (
                id, plan_id, description, dependencies, action_json,
                verification_json, status, result, error, max_retries,
                retry_delay_ms, snapshot_before, created_at, updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
            ON CONFLICT(plan_id, id) DO UPDATE SET
                description = excluded.description,
                dependencies = excluded.dependencies,
                action_json = excluded.action_json,
                verification_json = excluded.verification_json,
                status = excluded.status,
                result = excluded.result,
                error = excluded.error,
                max_retries = excluded.max_retries,
                retry_delay_ms = excluded.retry_delay_ms,
                snapshot_before = excluded.snapshot_before,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(&task.id)
        .bind(plan_id)
        .bind(&task.description)
        .bind(deps)
        .bind(action_json)
        .bind(verification_json)
        .bind(status_str)
        .bind(task.result.as_ref())
        .bind(task.error.as_ref())
        .bind(task.max_retries as i64)
        .bind(task.retry_delay.as_millis() as i64)
        .bind(if task.snapshot_before { 1i64 } else { 0i64 })
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: format!("Failed to save task '{}'", task.id),
            details: e.to_string(),
        })?;

        Ok(())
    }

    /// Update only the status (and optionally result/error) of a task.
    #[instrument(skip(self))]
    pub async fn update_task_status(
        &self,
        plan_id: &str,
        task_id: &str,
        status: TaskStatus,
        result: Option<&str>,
        error: Option<&str>,
    ) -> crate::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let status_str = format!("{:?}", status);

        sqlx::query(
            r#"
            UPDATE planner_tasks
            SET status = ?1, result = ?2, error = ?3, updated_at = ?4
            WHERE plan_id = ?5 AND id = ?6
            "#,
        )
        .bind(status_str)
        .bind(result)
        .bind(error)
        .bind(now)
        .bind(plan_id)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: format!(
                "Failed to update task status for '{}' in plan '{}'",
                task_id, plan_id
            ),
            details: e.to_string(),
        })?;

        Ok(())
    }

    /// Mark a plan as completed.
    #[instrument(skip(self))]
    pub async fn complete_plan(&self, plan_id: &str, success: bool) -> crate::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();

        sqlx::query(
            r#"
            UPDATE planner_plans
            SET completed_at = ?1, success = ?2
            WHERE id = ?3
            "#,
        )
        .bind(now)
        .bind(if success { 1i64 } else { 0i64 })
        .bind(plan_id)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: format!("Failed to complete plan '{}'", plan_id),
            details: e.to_string(),
        })?;

        Ok(())
    }

    /// Load a plan and all its tasks from the database.
    #[instrument(skip(self))]
    pub async fn load_plan(&self, plan_id: &str) -> crate::Result<Option<Plan>> {
        let row = sqlx::query("SELECT goal FROM planner_plans WHERE id = ?1")
            .bind(plan_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to load plan '{}'", plan_id),
                details: e.to_string(),
            })?;

        let goal: String = match row {
            Some(r) => r
                .try_get("goal")
                .map_err(|e| crate::error::SyscityError::Storage {
                    context: format!("Failed to read plan '{}' goal", plan_id),
                    details: e.to_string(),
                })?,
            None => return Ok(None),
        };

        let mut plan = Plan::new(goal);

        let task_rows = sqlx::query(
            r#"
            SELECT
                id, description, dependencies, action_json, verification_json,
                status, result, error, max_retries, retry_delay_ms, snapshot_before
            FROM planner_tasks
            WHERE plan_id = ?1
            "#,
        )
        .bind(plan_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: format!("Failed to load tasks for plan '{}'", plan_id),
            details: e.to_string(),
        })?;

        for row in task_rows {
            let id: String =
                row.try_get("id")
                    .map_err(|e| crate::error::SyscityError::Storage {
                        context: "Failed to read task id".to_string(),
                        details: e.to_string(),
                    })?;
            let description: String =
                row.try_get("description")
                    .map_err(|e| crate::error::SyscityError::Storage {
                        context: format!("Failed to read task '{}' description", id),
                        details: e.to_string(),
                    })?;
            let deps_json: String = row
                .try_get("dependencies")
                .unwrap_or_else(|_| "[]".to_string());
            let deps: Vec<String> = serde_json::from_str(&deps_json).unwrap_or_default();
            let action = row
                .try_get::<Option<String>, _>("action_json")
                .ok()
                .flatten()
                .and_then(|j| serde_json::from_str(&j).ok())
                .unwrap_or(crate::computer::DesktopAction::Wait { milliseconds: 0 });
            let verification = row
                .try_get::<Option<String>, _>("verification_json")
                .ok()
                .flatten()
                .and_then(|j| serde_json::from_str(&j).ok());
            let status_str: String = row
                .try_get("status")
                .unwrap_or_else(|_| "Pending".to_string());
            let status = parse_task_status(&status_str);
            let result: Option<String> = row.try_get("result").ok();
            let error: Option<String> = row.try_get("error").ok();
            let max_retries: i64 = row.try_get("max_retries").unwrap_or(2);
            let retry_delay_ms: i64 = row.try_get("retry_delay_ms").unwrap_or(1000);
            let snapshot_before: i64 = row.try_get("snapshot_before").unwrap_or(0);

            let mut task = Task {
                id,
                description,
                action,
                dependencies: deps,
                verification,
                snapshot_before: snapshot_before != 0,
                max_retries: max_retries as u32,
                retry_delay: Duration::from_millis(retry_delay_ms as u64),
                status,
                error,
                result,
            };

            // Ensure pending tasks that were running when we crashed go back to pending.
            if matches!(task.status, TaskStatus::Running) {
                task.status = TaskStatus::Pending;
            }

            plan.add_task(task);
        }

        Ok(Some(plan))
    }
}

/// Summary of an incomplete plan for startup recovery display.
#[derive(Debug, Clone)]
pub struct PlanSummary {
    pub id: String,
    pub goal: String,
    pub created_at: String,
    pub total_tasks: usize,
    pub completed_tasks: usize,
    pub failed_tasks: usize,
    pub pending_tasks: usize,
}

impl TaskStateStore {
    /// List all plan IDs that are not yet marked completed.
    pub async fn list_incomplete_plans(&self) -> crate::Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT id FROM planner_plans WHERE completed_at IS NULL ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to list incomplete plans".to_string(),
            details: e.to_string(),
        })?;

        Ok(rows
            .iter()
            .map(|r| r.try_get::<String, _>("id").unwrap_or_default())
            .filter(|s| !s.is_empty())
            .collect())
    }

    /// Load summaries for all incomplete plans (with task progress counts).
    pub async fn load_plan_summaries(&self) -> crate::Result<Vec<PlanSummary>> {
        let plan_rows = sqlx::query(
            r#"
            SELECT id, goal, created_at
            FROM planner_plans
            WHERE completed_at IS NULL
            ORDER BY created_at DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to load incomplete plan summaries".to_string(),
            details: e.to_string(),
        })?;

        let mut summaries = Vec::new();
        for row in plan_rows {
            let id: String = row.try_get("id").unwrap_or_default();
            let goal: String = row.try_get("goal").unwrap_or_default();
            let created_at: String = row.try_get("created_at").unwrap_or_default();

            let counts = sqlx::query(
                r#"
                SELECT status, COUNT(*) as cnt
                FROM planner_tasks
                WHERE plan_id = ?1
                GROUP BY status
                "#,
            )
            .bind(&id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to count tasks for plan '{}'", id),
                details: e.to_string(),
            })?;

            let mut total = 0usize;
            let mut completed = 0usize;
            let mut failed = 0usize;
            let mut pending = 0usize;

            for c in counts {
                let status_str: String = c.try_get("status").unwrap_or_default();
                let cnt: i64 = c.try_get("cnt").unwrap_or(0);
                let n = cnt as usize;
                total += n;
                match status_str.as_str() {
                    "Completed" | "completed" => completed += n,
                    "Failed" | "failed" | "RolledBack" | "rolled_back" => failed += n,
                    _ => pending += n,
                }
            }

            summaries.push(PlanSummary {
                id,
                goal,
                created_at,
                total_tasks: total,
                completed_tasks: completed,
                failed_tasks: failed,
                pending_tasks: pending,
            });
        }

        Ok(summaries)
    }

    /// Delete a plan and all its tasks.
    pub async fn delete_plan(&self, plan_id: &str) -> crate::Result<()> {
        sqlx::query("DELETE FROM planner_plans WHERE id = ?1")
            .bind(plan_id)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to delete plan '{}'", plan_id),
                details: e.to_string(),
            })?;
        Ok(())
    }
}

fn parse_task_status(s: &str) -> TaskStatus {
    match s {
        "Pending" | "pending" => TaskStatus::Pending,
        "Running" | "running" => TaskStatus::Running,
        "Completed" | "completed" => TaskStatus::Completed,
        "Failed" | "failed" => TaskStatus::Failed,
        "RolledBack" | "rolled_back" => TaskStatus::RolledBack,
        _ => TaskStatus::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_task_status() {
        assert!(matches!(parse_task_status("Pending"), TaskStatus::Pending));
        assert!(matches!(parse_task_status("Running"), TaskStatus::Running));
        assert!(matches!(parse_task_status("Completed"), TaskStatus::Completed));
        assert!(matches!(parse_task_status("Failed"), TaskStatus::Failed));
        assert!(matches!(parse_task_status("RolledBack"), TaskStatus::RolledBack));
        assert!(matches!(parse_task_status("unknown"), TaskStatus::Pending));
    }
}
