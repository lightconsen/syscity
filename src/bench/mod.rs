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
//! ```text
//! bench/osworld/run.py ──spawn──► syscity bench run --instruction ...
//!                                     │
//!                                     ├─ Agent (computer, screen_state, time, todo)
//!                                     └─ ComputerSession → OsworldDesktop → VM
//! ```

pub mod run;

pub use run::{run_once, BenchOptions, BenchResult, DEFAULT_MAX_STEPS, RESULT_SCHEMA};
