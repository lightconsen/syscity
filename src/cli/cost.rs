//! Cost-guard commands for Syscity
//!
//! Inspect and clear the live spend / action-rate guard that gates every
//! provider call (WS `cost.get` / `cost.reset`).

use clap::Subcommand;

use crate::cli::ws;
use crate::error::Result;

#[derive(Debug, Subcommand)]
pub enum CostCommands {
    /// Show the cost guard's limits and current usage
    Status,
    /// Clear a tripped cost guard so the agent resumes immediately
    Reset,
}

/// Run cost commands
pub async fn run_cost_command(command: &CostCommands) -> Result<()> {
    match command {
        CostCommands::Status => {
            let body = ws::call("cost.get", serde_json::json!({})).await?;
            let cents = body.get("daily_spend_cents").and_then(|v| v.as_u64());
            let daily_limit = body.get("daily_limit_cents").and_then(|v| v.as_u64());
            let actions = body.get("hourly_actions").and_then(|v| v.as_u64());
            let action_limit = body.get("hourly_action_limit").and_then(|v| v.as_u64());
            let exceeded = body
                .get("exceeded")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            println!("Cost guard");
            println!("  Daily spend:    {} of {}", dollars(cents), limit_display(daily_limit));
            println!(
                "  Hourly calls:   {} of {}",
                actions.unwrap_or(0),
                // A count, not cents — "600" or "unlimited".
                match action_limit {
                    Some(0) | None => "unlimited".to_string(),
                    Some(n) => n.to_string(),
                }
            );
            if exceeded {
                println!();
                println!("  ⚠ A limit is tripped — provider calls are being refused.");
                println!(
                    "    It clears when the window rolls over, or now with: syscity cost reset"
                );
            }

            // Per-agent shares. Printed only when there are any: a
            // single-operator deployment has nothing to say here, and a
            // deployment that does needs this to answer "who exhausted the
            // budget" without opening the config.
            if let Some(agents) = body.get("agents").and_then(|v| v.as_array()) {
                let lines = agent_lines(agents);
                if !lines.is_empty() {
                    println!();
                    println!("Per agent");
                    for line in lines {
                        println!("{line}");
                    }
                }
            }
            Ok(())
        }
        CostCommands::Reset => {
            let body = ws::call("cost.reset", serde_json::json!({})).await?;
            let was_exceeded = body
                .get("was_exceeded")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if was_exceeded {
                println!("Cost guard cleared — the agent will accept provider calls again.");
            } else {
                println!("Cost guard was not tripped; nothing to clear.");
            }
            Ok(())
        }
    }
}

/// Render the per-agent rows of `cost.get`.
///
/// An agent is worth a line when it carries a cap or has spent something: an
/// agent that is neither is an entry for a deployment with nothing to say.
fn agent_lines(agents: &[serde_json::Value]) -> Vec<String> {
    let numbered = |v: Option<u64>| match v {
        Some(0) | None => "unlimited".to_string(),
        Some(n) => n.to_string(),
    };
    agents
        .iter()
        .filter_map(|a| {
            let id = a.get("agent_id").and_then(|v| v.as_str()).unwrap_or("?");
            let spend = a
                .get("daily_spend_cents")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let calls = a
                .get("hourly_actions")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let daily_limit = a
                .get("daily_limit_cents")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let action_limit = a
                .get("hourly_action_limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if spend == 0 && calls == 0 && daily_limit == 0 && action_limit == 0 {
                return None;
            }
            let tripped = a.get("exceeded").and_then(|v| v.as_bool()).unwrap_or(false);
            Some(format!(
                "  {}{id}: {} of {} · {} of {} calls",
                if tripped { "⚠ " } else { "" },
                dollars(Some(spend)),
                limit_display(Some(daily_limit)),
                calls,
                numbered(Some(action_limit)),
            ))
        })
        .collect()
}

/// Render a cent amount as dollars.
fn dollars(cents: Option<u64>) -> String {
    format!("${:.2}", cents.unwrap_or(0) as f64 / 100.0)
}

/// Render a **cent** limit as dollars, where 0 means "no limit".
fn limit_display(limit: Option<u64>) -> String {
    match limit {
        Some(0) | None => "unlimited".to_string(),
        Some(cents) => format!("${:.2}", cents as f64 / 100.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_render_zero_as_unlimited() {
        assert_eq!(limit_display(Some(0)), "unlimited");
        assert_eq!(limit_display(None), "unlimited");
        assert_eq!(limit_display(Some(500)), "$5.00");
    }

    #[test]
    fn cents_render_as_dollars() {
        assert_eq!(dollars(Some(0)), "$0.00");
        assert_eq!(dollars(Some(1234)), "$12.34");
        assert_eq!(dollars(None), "$0.00");
    }

    /// An agent with neither a cap nor any spend is noise; one with either is
    /// exactly what the operator came to read.
    #[test]
    fn agent_lines_show_only_agents_with_something_to_say() {
        let agents = vec![
            serde_json::json!({
                "agent_id": "idle", "daily_spend_cents": 0, "daily_limit_cents": 0,
                "hourly_actions": 0, "hourly_action_limit": 0, "exceeded": false,
            }),
            serde_json::json!({
                "agent_id": "spender", "daily_spend_cents": 300, "daily_limit_cents": 0,
                "hourly_actions": 4, "hourly_action_limit": 0, "exceeded": false,
            }),
            serde_json::json!({
                "agent_id": "capped", "daily_spend_cents": 500, "daily_limit_cents": 500,
                "hourly_actions": 9, "hourly_action_limit": 10, "exceeded": true,
            }),
        ];

        let lines = agent_lines(&agents);

        assert_eq!(lines.len(), 2, "the idle agent is dropped");
        assert_eq!(lines[0], "  spender: $3.00 of unlimited · 4 of unlimited calls");
        assert_eq!(
            lines[1], "  ⚠ capped: $5.00 of $5.00 · 9 of 10 calls",
            "a tripped agent is marked"
        );
    }
}
