//! Benchmark entry point — one instruction, one remote desktop, one result
//! file.
//!
//! This is the Rust side of [`docs/computer-use-benchmark.md`]: the benchmark
//! harness (OSWorld, Cua Bench) owns the machine and the evaluator, and calls
//! [`run::run_once`] once per task with the instruction it wants attempted.
//! Everything between — the system prompt, the tool set, the action ladder,
//! element ids, the effect verification — is the product being measured, so it
//! runs unmodified.
//!
//! What is deliberately *not* here: any scoring. A benchmark number must come
//! from the benchmark's own evaluator; a second judge invented on this side
//! would produce a number nobody else can compare against.
//!
//! Two shapes drive a task, and they differ only in who owns the step:
//!
//! ```text
//! bench/osworld/run.py ──spawn──► syscity bench run --instruction ...
//!                                     │
//!                                     ├─ Agent (computer, screen_state, time, todo)
//!                                     └─ ComputerSession → OsworldDesktop → VM
//!
//! bench/osworld/run.py ──stdio──► syscity bench serve
//!                                     └─ the same agent, with actions routed
//!                                        through the harness's own env.step
//! ```
//!
//! `bench run` performs the whole task and leaves a replayable action log;
//! `bench serve` (see [`serve`]) hands one action at a time to the harness and
//! waits for it, which is the interleaving OSWorld's loop expects; `bench
//! probe` (see [`probe`]) asks the desktop whether it is ready at all.

// INVARIANTS-NONE: the benchmark drives the same agent and tool stack as a
// normal session — the runtime invariants live with those components. Nothing
// here owns shared state: `run` and `serve` each build a session and drop it,
// and `probe` only issues reads against a remote desktop. Scoring is the
// benchmark's evaluator, not ours (see above), so there is no counter to keep.

pub mod probe;
pub mod run;
pub mod serve;

#[cfg(test)]
pub(crate) mod testing;

pub use probe::{probe, ProbeOptions, ProbeReport, PROBE_SCHEMA};
pub use run::{run_once, BenchOptions, BenchResult, DEFAULT_MAX_STEPS, RESULT_SCHEMA};
pub use serve::{serve, SERVE_SCHEMA};
