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
}
