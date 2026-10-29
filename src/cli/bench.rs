//! `syscity bench` — drive one benchmark task over a remote desktop.
//!
//! Called once per task by the benchmark glue (`bench/osworld/run.py`,
//! `bench/cua/sycity_agent.py`). The design and the output contract live in
//! [`docs/computer-use-benchmark.md`](../../docs/computer-use-benchmark.md).
//!
//! ```bash
//! syscity bench run \
//!   --instruction "Export the document as PDF" \
//!   --computer-url http://10.0.2.15:5000 \
//!   --task-id 7d8a1c \
//!   --max-steps 60 \
//!   --out /tmp/traj/7d8a1c/result.json
//! ```

use std::path::PathBuf;

use clap::Subcommand;

use crate::bench::{self, BenchOptions};
use crate::error::SyscityError;
use crate::Result;

/// Bench subcommands
#[derive(Debug, Clone, Subcommand)]
pub enum BenchCommands {
    /// Run one benchmark instruction against a remote desktop
    Run {
        /// The benchmark's instruction, verbatim
        #[arg(long)]
        instruction: String,
        /// Base URL of the desktop service (OSWorld VM: `http://<vm_ip>:5000`)
        #[arg(long)]
        computer_url: String,
        /// Benchmark task id — names the conversation and the output file
        #[arg(long)]
        task_id: String,
        /// Tool-iteration budget (default 60, or `SYSCITY_MAX_TOOL_ITERATIONS`)
        #[arg(long)]
        max_steps: Option<usize>,
        /// Write the result JSON (schema 1) here
        #[arg(long)]
        out: Option<PathBuf>,
        /// LLM provider preset (e.g. "deepseek", "anthropic")
        #[arg(long)]
        provider: Option<String>,
        /// Model name (defaults to the provider's)
        #[arg(long)]
        model: Option<String>,
        /// API key (overrides env var / config file)
        #[arg(long)]
        api_key: Option<String>,
        /// API base URL (for custom/self-hosted providers)
        #[arg(long)]
        base_url: Option<String>,
        /// Bearer token for the desktop service, when it asks for one
        #[arg(long)]
        auth_token: Option<String>,
    },
    /// Check that a remote desktop answers in the shape the bench expects
    ///
    /// Run this before a task, not after: it reports each endpoint in seconds
    /// and spends no model time. Exits non-zero when any check fails.
    Probe {
        /// Base URL of the desktop service (OSWorld VM: `http://<vm_ip>:5000`)
        #[arg(long)]
        computer_url: String,
        /// Bearer token for the desktop service, when it asks for one
        #[arg(long)]
        auth_token: Option<String>,
        /// Also run a read-only snippet through `/execute` (proves pyautogui)
        #[arg(long)]
        execute: bool,
        /// Print the report as JSON instead of lines
        #[arg(long)]
        json: bool,
    },
    /// Step one task over stdin/stdout, one action per harness `env.step`
    ///
    /// The harness owns the loop: it sends `{"type":"task",...}`, receives one
    /// `{"type":"action",...}` at a time, answers `{"type":"result",...}` after
    /// executing it, and reads the final `{"type":"done",...}`.
    Serve {
        /// Base URL of the desktop service (OSWorld VM: `http://<vm_ip>:5000`)
        #[arg(long)]
        computer_url: String,
        /// Tool-iteration budget (default 60, or `SYSCITY_MAX_TOOL_ITERATIONS`)
        #[arg(long)]
        max_steps: Option<usize>,
        /// LLM provider preset (e.g. "deepseek", "anthropic")
        #[arg(long)]
        provider: Option<String>,
        /// Model name (defaults to the provider's)
        #[arg(long)]
        model: Option<String>,
        /// API key (overrides env var / config file)
        #[arg(long)]
        api_key: Option<String>,
        /// API base URL (for custom/self-hosted providers)
        #[arg(long)]
        base_url: Option<String>,
        /// Bearer token for the desktop service, when it asks for one
        #[arg(long)]
        auth_token: Option<String>,
    },
}

/// Dispatch a `syscity bench` subcommand.
pub async fn run_bench_command(command: BenchCommands) -> Result<()> {
    match command {
        BenchCommands::Run {
            instruction,
            computer_url,
            task_id,
            max_steps,
            out,
            provider,
            model,
            api_key,
            base_url,
            auth_token,
        } => {
            let result = bench::run_once(BenchOptions {
                instruction,
                computer_url,
                task_id,
                max_steps,
                out,
                provider,
                model,
                api_key,
                base_url,
                auth_token,
            })
            .await?;

            // Human-readable view for whoever is tailing the harness log; the
            // machine-readable contract is the JSON file.
            println!("### ANSWER ###");
            println!("{}", result.answer);
            println!("### END ANSWER ###");

            if let Some(error) = &result.error {
                // The result file is already on disk; a non-zero exit tells the
                // harness this task errored rather than finished.
                return Err(SyscityError::Validation(format!(
                    "bench task {} failed: {error}",
                    result.task_id
                )));
            }
            Ok(())
        }
        BenchCommands::Probe {
            computer_url,
            auth_token,
            execute,
            json,
        } => {
            let report = bench::probe(&bench::ProbeOptions {
                computer_url,
                auth_token,
                execute,
            })
            .await?;

            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", bench::probe::render(&report));
            }

            if !report.ok {
                // The report is already on stdout; a non-zero exit is what a
                // script (or a person reading CI) acts on.
                return Err(SyscityError::Validation(format!(
                    "the desktop at {} failed {} check(s)",
                    report.computer_url,
                    report.failures.len()
                )));
            }
            Ok(())
        }
        BenchCommands::Serve {
            computer_url,
            max_steps,
            provider,
            model,
            api_key,
            base_url,
            auth_token,
        } => {
            // Everything the task needs (instruction, id) arrives on the wire;
            // the run reports itself with a `done` line, so there is nothing to
            // print here that the harness does not already have.
            bench::serve(BenchOptions {
                computer_url,
                max_steps,
                provider,
                model,
                api_key,
                base_url,
                auth_token,
                ..Default::default()
            })
            .await?;
            Ok(())
        }
    }
}
