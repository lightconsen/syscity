//! Eval CLI subcommand — load, validate, and run evaluation suites.
//!
//! ```bash
//! syscity eval list               # List all available suites
//! syscity eval validate           # Validate all YAML task files
//! syscity eval run <suite>        # Load + dry-run a suite
//! syscity eval run <suite> --full # Run a suite with a live Agent (standalone, no daemon needed)
//! ```

use std::path::PathBuf;

use clap::Subcommand;

use crate::config::Config;
use crate::eval;
use crate::Result;

/// Eval subcommands
#[derive(Debug, Subcommand)]
pub enum EvalCommands {
    /// List all available evaluation suites
    List {
        /// Path to evals directory (defaults to `./evals`)
        #[arg(short, long)]
        dir: Option<PathBuf>,
    },
    /// Validate all YAML task files in the evals directory
    Validate {
        /// Path to evals directory (defaults to `./evals`)
        #[arg(short, long)]
        dir: Option<PathBuf>,
    },
    /// Load and run an evaluation suite
    Run {
        /// Suite name (e.g. "ci_smoke", "capability", "regression")
        suite: String,
        /// Path to evals directory (defaults to `./evals`)
        #[arg(short, long)]
        dir: Option<PathBuf>,
        /// Actually execute tasks (standalone, no daemon needed)
        #[arg(long)]
        full: bool,
        /// Number of trials per task
        #[arg(short, long)]
        trials: Option<usize>,
        /// LLM provider (e.g. "anthropic", "openai")
        #[arg(long)]
        provider: Option<String>,
        /// Model name (e.g. "claude-sonnet-4-20250514")
        #[arg(long)]
        model: Option<String>,
        /// API key (overrides env var / config file)
        #[arg(long)]
        api_key: Option<String>,
        /// API base URL (for custom/self-hosted providers)
        #[arg(long)]
        base_url: Option<String>,
        /// Show detailed skill evaluation breakdown per dimension
        #[arg(long)]
        skill_breakdown: bool,
        /// Collect badcases from failed trials into evals/badcases/
        #[arg(long)]
        collect_badcases: bool,
        /// Fraction of tasks to run (0.0–1.0). 1.0 = all tasks (§10).
        #[arg(long)]
        sampling_rate: Option<f64>,
        /// Re-run only these task ids (comma-separated); all other tasks are
        /// skipped. Lets a full-gate failure be re-run on just the failing
        /// tasks with identical judge/conditions semantics.
        #[arg(long, value_delimiter = ',')]
        only: Option<Vec<String>>,
        /// Judge (Critic) provider preset (defaults to the agent's provider)
        #[arg(long)]
        judge_provider: Option<String>,
        /// Judge model (e.g. a cheaper/faster model than the agent's)
        #[arg(long)]
        judge_model: Option<String>,
        /// Judge API key (overrides env var / agent key)
        #[arg(long)]
        judge_api_key: Option<String>,
        /// Judge API base URL
        #[arg(long)]
        judge_base_url: Option<String>,
    },
    /// Run the governed badcase regression suite (expiry/dedup/difficulty
    /// weighting applied from cfg.eval.badcase_governance, or defaults)
    BadcaseRun {
        /// Path to evals directory (defaults to `./evals`)
        #[arg(short, long)]
        dir: Option<PathBuf>,
        /// Number of trials per task (overrides governance weighting)
        #[arg(short, long)]
        trials: Option<usize>,
        /// LLM provider (e.g. "anthropic", "openai")
        #[arg(long)]
        provider: Option<String>,
        /// Model name (e.g. "claude-sonnet-4-20250514")
        #[arg(long)]
        model: Option<String>,
        /// API key (overrides env var / config file)
        #[arg(long)]
        api_key: Option<String>,
        /// API base URL (for custom/self-hosted providers)
        #[arg(long)]
        base_url: Option<String>,
        /// Show detailed skill evaluation breakdown per dimension
        #[arg(long)]
        skill_breakdown: bool,
        /// Collect new badcases from failed trials into evals/badcases/
        #[arg(long)]
        collect_badcases: bool,
        /// Fraction of tasks to run (0.0–1.0). 1.0 = all tasks (§10).
        #[arg(long)]
        sampling_rate: Option<f64>,
        /// Judge (Critic) provider preset (defaults to the agent's provider)
        #[arg(long)]
        judge_provider: Option<String>,
        /// Judge model (e.g. a cheaper/faster model than the agent's)
        #[arg(long)]
        judge_model: Option<String>,
        /// Judge API key (overrides env var / agent key)
        #[arg(long)]
        judge_api_key: Option<String>,
        /// Judge API base URL
        #[arg(long)]
        judge_base_url: Option<String>,
    },
    /// List collected badcases
    BadcaseList {
        /// Path to evals directory (defaults to `./evals`)
        #[arg(short, long)]
        dir: Option<PathBuf>,
        /// Show RCA details
        #[arg(long)]
        verbose: bool,
        /// Show badcase clusters grouped by phenomenon × module
        #[arg(long)]
        cluster: bool,
    },
    /// Show details of a specific badcase file
    BadcaseShow {
        /// Badcase task file stem (e.g. "tool_selection")
        filter: String,
        /// Path to evals directory (defaults to `./evals`)
        #[arg(short, long)]
        dir: Option<PathBuf>,
    },
    /// Manually submit a badcase for RCA and regression tracking
    BadcaseSubmit {
        /// Eval task ID
        #[arg(long)]
        task_id: String,
        /// The user input that caused the failure
        #[arg(long)]
        input: String,
        /// The agent's response
        #[arg(long)]
        response: String,
        /// Optional description
        #[arg(long)]
        description: Option<String>,
        /// Optional failure reason
        #[arg(long)]
        failure_reason: Option<String>,
        /// Path to evals directory (defaults to `./evals`)
        #[arg(short, long)]
        dir: Option<PathBuf>,
    },
    /// List and manage human review cases
    Review {
        /// Path to evals directory (defaults to `./evals`)
        #[arg(short, long)]
        dir: Option<PathBuf>,
        /// Show only pending cases
        #[arg(long)]
        pending: bool,
        /// Show detailed case content
        #[arg(long)]
        verbose: bool,
        /// Mark a specific case as reviewed (by filename)
        #[arg(long)]
        mark_reviewed: Option<String>,
    },
    /// List and generate action items from badcase RCA results
    ActionItems {
        /// Path to evals directory (defaults to `./evals`)
        #[arg(short, long)]
        dir: Option<PathBuf>,
        /// Generate action items by extracting RCA results from badcases
        #[arg(long)]
        generate: bool,
        /// Show detailed action item information
        #[arg(long)]
        verbose: bool,
    },
    /// View feedback pipeline status (eval/ops/model channels)
    Feedback {
        /// Path to evals directory (defaults to `./evals`)
        #[arg(short, long)]
        dir: Option<PathBuf>,
        /// Channel: "eval", "ops", or "model"
        channel: Option<String>,
        /// Show verbose details per channel
        #[arg(long)]
        verbose: bool,
    },
    /// Run Critic calibration against known-answer cases
    Calibrate {
        /// Path to evals directory (defaults to `./evals`)
        #[arg(short, long)]
        dir: Option<PathBuf>,
        /// Calibration YAML file (defaults to `evals/calibration/default.yaml`)
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Show calibration history instead of running
        #[arg(long)]
        history: bool,
        /// Check for drift compared to previous run
        #[arg(long)]
        drift: bool,
        /// LLM provider (e.g. "anthropic", "openai")
        #[arg(long)]
        provider: Option<String>,
        /// Model name for the Critic
        #[arg(long)]
        model: Option<String>,
        /// API key
        #[arg(long)]
        api_key: Option<String>,
        /// API base URL
        #[arg(long)]
        base_url: Option<String>,
    },
}

impl EvalCommands {
    pub async fn run(&self, config: &Config) -> Result<()> {
        match self {
            Self::List { dir } => cmd_list(dir.clone()).await,
            Self::Validate { dir } => cmd_validate(dir.clone()).await,
            Self::Run {
                suite,
                dir,
                full,
                trials,
                provider,
                model,
                api_key,
                base_url,
                skill_breakdown,
                collect_badcases,
                sampling_rate,
                only,
                judge_provider,
                judge_model,
                judge_api_key,
                judge_base_url,
            } => {
                cmd_run(
                    config,
                    suite.clone(),
                    *full,
                    eval::standalone::SuiteRunOptions {
                        evals_dir: dir.clone(),
                        trials_override: *trials,
                        sampling_rate_override: *sampling_rate,
                        skill_breakdown: *skill_breakdown,
                        collect_badcases: *collect_badcases,
                        only_tasks: only.clone(),
                        judge: eval::standalone::ProviderSelection {
                            provider: judge_provider.clone(),
                            model: judge_model.clone(),
                            api_key: judge_api_key.clone(),
                            base_url: judge_base_url.clone(),
                        },
                    },
                    eval::standalone::ProviderSelection {
                        provider: provider.clone(),
                        model: model.clone(),
                        api_key: api_key.clone(),
                        base_url: base_url.clone(),
                    },
                )
                .await
            }
            Self::BadcaseRun {
                dir,
                trials,
                provider,
                model,
                api_key,
                base_url,
                skill_breakdown,
                collect_badcases,
                sampling_rate,
                judge_provider,
                judge_model,
                judge_api_key,
                judge_base_url,
            } => {
                cmd_badcase_run(
                    config,
                    eval::standalone::SuiteRunOptions {
                        evals_dir: dir.clone(),
                        trials_override: *trials,
                        sampling_rate_override: *sampling_rate,
                        skill_breakdown: *skill_breakdown,
                        collect_badcases: *collect_badcases,
                        only_tasks: None,
                        judge: eval::standalone::ProviderSelection {
                            provider: judge_provider.clone(),
                            model: judge_model.clone(),
                            api_key: judge_api_key.clone(),
                            base_url: judge_base_url.clone(),
                        },
                    },
                    eval::standalone::ProviderSelection {
                        provider: provider.clone(),
                        model: model.clone(),
                        api_key: api_key.clone(),
                        base_url: base_url.clone(),
                    },
                )
                .await
            }
            Self::BadcaseList { dir, verbose, cluster } => {
                cmd_badcase_list(dir.clone(), *verbose, *cluster).await
            }
            Self::BadcaseShow { filter, dir } => {
                cmd_badcase_show(filter.clone(), dir.clone()).await
            }
            Self::BadcaseSubmit {
                task_id,
                input,
                response,
                description,
                failure_reason,
                dir,
            } => {
                cmd_badcase_submit(
                    task_id.clone(),
                    input.clone(),
                    response.clone(),
                    description.clone(),
                    failure_reason.clone(),
                    dir.clone(),
                )
                .await
            }
            Self::Review {
                dir,
                pending,
                verbose,
                mark_reviewed,
            } => cmd_review(dir.clone(), *pending, *verbose, mark_reviewed.clone()).await,
            Self::ActionItems { dir, generate, verbose } => {
                cmd_action_items(dir.clone(), *generate, *verbose).await
            }
            Self::Feedback { dir, channel, verbose } => {
                cmd_feedback(dir.clone(), channel.clone(), *verbose).await
            }
            Self::Calibrate {
                dir,
                file,
                history,
                drift,
                provider,
                model,
                api_key,
                base_url,
            } => {
                cmd_calibrate(
                    dir.clone(),
                    file.clone(),
                    *history,
                    *drift,
                    eval::standalone::ProviderSelection {
                        provider: provider.clone(),
                        model: model.clone(),
                        api_key: api_key.clone(),
                        base_url: base_url.clone(),
                    },
                )
                .await
            }
        }
    }
}

/// `eval list` — show all available suites.
async fn cmd_list(dir: Option<PathBuf>) -> Result<()> {
    let evals_dir = dir.unwrap_or_else(eval::default_evals_dir);
    let suites = eval::list_suites(&evals_dir)?;

    if suites.is_empty() {
        println!("No suites found in {:?}", evals_dir.join("suites"));
        println!("  (expected .yaml files in evals/suites/)");
        return Ok(());
    }

    println!("Available suites:");
    for (name, display) in &suites {
        println!("  {:20}  {}", name, display);
    }
    Ok(())
}

/// `eval validate` — check all YAML files parse correctly.
async fn cmd_validate(dir: Option<PathBuf>) -> Result<()> {
    let evals_dir = dir.unwrap_or_else(eval::default_evals_dir);
    let mut total_files = 0usize;
    let mut total_tasks = 0usize;
    let mut errors = Vec::new();

    // Walk YAML files in evals/ recursively
    collect_yaml_files(&evals_dir, &mut Vec::new(), &mut |path| {
        // Check if it's a suite manifest (in suites/) or a task file
        let is_suite = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n == "suites")
            .unwrap_or(false);

        if is_suite {
            // Validate suite manifest structure
            let content = std::fs::read_to_string(path).map_err(crate::error::SyscityError::Io)?;
            match serde_norway::from_str::<serde_norway::Value>(&content) {
                Ok(_) => {
                    total_files += 1;
                    if let Some(name) = path.file_name() {
                        println!("  ✓ {} (suite)", name.to_string_lossy());
                    }
                }
                Err(e) => {
                    errors.push(format!("{}: {}", path.display(), e));
                }
            }
        } else {
            // Validate task file — parse as EvalTask
            match eval::load_tasks(path) {
                Ok(loaded) => {
                    total_files += 1;
                    total_tasks += loaded.tasks.len();
                    if let Some(name) = path.file_name() {
                        println!("  ✓ {} ({} tasks)", name.to_string_lossy(), loaded.tasks.len());
                    }
                }
                Err(e) => {
                    errors.push(format!("{}: {}", path.display(), e));
                }
            }
        }
        Ok::<_, crate::error::SyscityError>(())
    })?;

    println!();
    if errors.is_empty() {
        println!(
            "✅ All {} YAML files valid ({} tasks across {} files)",
            total_files, total_tasks, total_files
        );
    } else {
        for err in &errors {
            eprintln!("❌ {}", err);
        }
        eprintln!("❌ {}/{} files have errors", errors.len(), total_files + errors.len());
    }

    Ok(())
}

/// `eval run <suite>` — load and optionally execute a suite.
async fn cmd_run(
    config: &Config,
    suite: String,
    full: bool,
    opts: eval::standalone::SuiteRunOptions,
    selection: eval::standalone::ProviderSelection,
) -> Result<()> {
    let evals_dir = opts
        .evals_dir
        .clone()
        .unwrap_or_else(eval::default_evals_dir);
    let manifest_path = evals_dir.join("suites").join(format!("{}.yaml", suite));

    if !manifest_path.exists() {
        eprintln!("❌ Suite '{}' not found at {:?}", suite, manifest_path);
        eprintln!("   Run `syscity eval list` to see available suites.");
        return Ok(());
    }

    let mut eval_suite = eval::load_suite(&manifest_path, &suite)?;

    if full {
        // Standalone eval mode (no daemon needed)
        return eval::standalone::run_standalone_suite(
            config,
            &suite,
            eval::standalone::SuiteRunOptions {
                evals_dir: Some(evals_dir),
                ..opts
            },
            selection,
        )
        .await;
    }

    // Dry-run: print suite details (respect --only for the preview too)
    if let Some(ids) = &opts.only_tasks {
        if !ids.is_empty() {
            let wanted: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
            eval_suite.tasks.retain(|t| wanted.contains(t.id.as_str()));
        }
    }
    println!("═══ Eval Suite: {} ═══", eval_suite.name);
    println!("  ID:       {}", eval_suite.id);
    println!("  Category: {:?}", eval_suite.category);
    println!("  Tasks:    {}", eval_suite.tasks.len());
    println!("  Min pass: {:.0}%", eval_suite.min_pass_rate * 100.0);
    println!("  Trials:   {}", eval_suite.trials);
    println!();

    for task in &eval_suite.tasks {
        println!("  Task: {} — {}", task.id, task.description);
        println!("    Input: {}", task.input.chars().take(80).collect::<String>());
        println!("    Conditions: {}", task.conditions.len());
        if task.criteria.is_some() {
            println!("    Criteria: yes");
        }
        if !task.setup.is_empty() {
            println!("    Setup: {} commands", task.setup.len());
        }
        if !task.cleanup.is_empty() {
            println!("    Cleanup: {} commands", task.cleanup.len());
        }
        println!();
    }

    println!(
        "✓ Suite loaded successfully. {} tasks, {} trials each.",
        eval_suite.tasks.len(),
        eval_suite.trials
    );
    println!("  Use --full flag to execute (standalone, no daemon needed).");

    Ok(())
}

/// `eval badcase-run` — run the governed badcase regression suite.
///
/// Governance (expiry / dedup / downgrade / difficulty-weighted trials) comes
/// from `cfg.eval.badcase_governance` when a config file is present, else code
/// defaults. Delegates execution to the standalone harness core.
async fn cmd_badcase_run(
    config: &Config,
    opts: eval::standalone::SuiteRunOptions,
    selection: eval::standalone::ProviderSelection,
) -> Result<()> {
    eval::standalone::run_governed_badcase_suite(config, opts, selection).await
}

/// `eval badcase-list` — show collected badcases.
async fn cmd_badcase_list(dir: Option<PathBuf>, verbose: bool, cluster: bool) -> Result<()> {
    let evals_dir = dir.unwrap_or_else(eval::default_evals_dir);
    let badcases_dir = evals_dir.join("badcases");

    if !badcases_dir.is_dir() {
        println!("No badcases directory found at {:?}", badcases_dir);
        println!("  Run `syscity eval run <suite> --full --collect-badcases` to collect badcases.");
        return Ok(());
    }

    // ── Cluster mode ───────────────────────────────────────────────
    if cluster {
        return cmd_badcase_clusters(badcases_dir).await;
    }

    let mut files: Vec<_> = std::fs::read_dir(&badcases_dir)
        .map_err(crate::error::SyscityError::Io)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "yaml").unwrap_or(false))
        .collect();
    files.sort_by_key(|e| e.path());

    if files.is_empty() {
        println!("No badcase files found in {:?}", badcases_dir);
        return Ok(());
    }

    println!("Badcases in {:?}:", badcases_dir);
    for entry in &files {
        let path = entry.path();
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        match eval::load_tasks(&path) {
            Ok(loaded) => {
                let badcase_count = loaded
                    .tasks
                    .iter()
                    .filter(|t| t.failure_reason.is_some())
                    .count();
                println!(
                    "  {:30} {} tasks ({} badcases)",
                    file_name,
                    loaded.tasks.len(),
                    badcase_count
                );
                if verbose {
                    for task in &loaded.tasks {
                        let reason = task.failure_reason.as_deref().unwrap_or("none");
                        println!("      {} — {}", task.id, reason);
                    }
                }
            }
            Err(e) => {
                println!("  {:30} error: {}", file_name, e);
            }
        }
    }

    Ok(())
}

/// Show badcase clusters grouped by failure reason.
async fn cmd_badcase_clusters(badcases_dir: std::path::PathBuf) -> Result<()> {
    use std::collections::BTreeMap;

    let mut files: Vec<_> = std::fs::read_dir(&badcases_dir)
        .map_err(crate::error::SyscityError::Io)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "yaml").unwrap_or(false))
        .collect();
    files.sort_by_key(|e| e.path());

    // Group tasks by failure_reason
    let mut clusters: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut total = 0usize;

    for entry in &files {
        let path = entry.path();
        if let Ok(loaded) = eval::load_tasks(&path) {
            for task in &loaded.tasks {
                let reason = task
                    .failure_reason
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());
                clusters.entry(reason).or_default().push(task.id.clone());
                total += 1;
            }
        }
    }

    if clusters.is_empty() {
        println!("No badcases found in {:?}", badcases_dir);
        return Ok(());
    }

    // Sort clusters by size (descending)
    let mut sorted: Vec<_> = clusters.into_iter().collect();
    sorted.sort_by_key(|(_, task_ids)| std::cmp::Reverse(task_ids.len()));

    println!("═══ Badcase Clusters ({}) ═══", total);
    for (reason, task_ids) in &sorted {
        println!("  [{:>2}] {}", task_ids.len(), reason.chars().take(100).collect::<String>());
        for tid in task_ids {
            println!("         └ {}", tid);
        }
    }
    Ok(())
}

/// `eval badcase-show` — display a specific badcase file.
async fn cmd_badcase_show(filter: String, dir: Option<PathBuf>) -> Result<()> {
    let evals_dir = dir.unwrap_or_else(eval::default_evals_dir);
    let badcases_dir = evals_dir.join("badcases");
    let file_path = badcases_dir.join(format!("{}.yaml", filter));

    if !file_path.exists() {
        // Try fuzzy match
        let mut found = false;
        if badcases_dir.is_dir() {
            for entry in std::fs::read_dir(&badcases_dir).map_err(crate::error::SyscityError::Io)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().map(|e| e == "yaml").unwrap_or(false) {
                    if let Some(stem) = path.file_stem() {
                        if stem.to_string_lossy().contains(&filter) {
                            let content = std::fs::read_to_string(&path)
                                .map_err(crate::error::SyscityError::Io)?;
                            println!("=== {} ===\n{}", path.display(), content);
                            found = true;
                            break;
                        }
                    }
                }
            }
        }
        if !found {
            eprintln!("❌ No badcase file matching '{}' found in {:?}", filter, badcases_dir);
        }
        return Ok(());
    }

    let content = std::fs::read_to_string(&file_path).map_err(crate::error::SyscityError::Io)?;
    println!("=== {} ===\n{}", file_path.display(), content);
    Ok(())
}

/// `eval badcase-submit` — manually submit a badcase.
async fn cmd_badcase_submit(
    task_id: String,
    input: String,
    response: String,
    description: Option<String>,
    failure_reason: Option<String>,
    dir: Option<PathBuf>,
) -> Result<()> {
    use std::time::SystemTime;

    use crate::eval::rca::BadcaseEntry;
    use crate::eval::recycle::{write_badcase_yaml, BadcaseFixStatus, BadcaseRecord};

    let evals_dir = dir.unwrap_or_else(eval::default_evals_dir);
    let badcases_dir = evals_dir.join("badcases");
    std::fs::create_dir_all(&badcases_dir)?;

    // Build a minimal EvalTask for the YAML writer
    let stub_task = crate::eval::EvalTask {
        id: task_id.clone(),
        input: input.clone(),
        description: description.unwrap_or_default(),
        ..Default::default()
    };

    let record = BadcaseRecord {
        id: format!("manual_{}_{}", task_id, chrono_timestamp()),
        task_id,
        input,
        description: stub_task.description.clone(),
        failure_reason: failure_reason.unwrap_or_else(|| "Manual submission".into()),
        response,
        difficulty: "medium".into(),
        coverage: Vec::new(),
        rca_performed: false,
        rca_result: None,
        collected_at: SystemTime::now(),
        fix_status: BadcaseFixStatus::Unconfirmed,
        entry: BadcaseEntry::ManualSubmit {
            reporter: "cli".into(),
            description: "Submitted via syscity eval badcase-submit".into(),
        },
    };

    let path = write_badcase_yaml(&record, &stub_task, &badcases_dir)?;
    println!("✓ Badcase submitted: {:?}", path);
    Ok(())
}

/// Generate a compact timestamp for manual submission IDs.
fn chrono_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:x}", dur.as_secs())
}

/// `eval action-items` — list or generate action items from badcase RCA
/// results.
async fn cmd_action_items(dir: Option<PathBuf>, generate: bool, verbose: bool) -> Result<()> {
    use crate::eval::action::{generate_action_items, load_action_items, write_action_items};
    use crate::eval::recycle::extract_rca_results_from_badcases;

    let evals_dir = dir.unwrap_or_else(eval::default_evals_dir);
    let actions_dir = evals_dir.join("actions");

    // ── Generate mode ────────────────────────────────────────────────
    if generate {
        let results = extract_rca_results_from_badcases(&evals_dir)?;
        if results.is_empty() {
            println!("No RCA results found in badcases at {:?}", evals_dir.join("badcases"));
            println!("  Run `syscity eval run <suite> --full --collect-badcases` first.");
            return Ok(());
        }

        let items = generate_action_items(&results);
        let path = write_action_items(&items, &actions_dir)?;

        println!("Generated {} action items from {} RCA results", items.len(), results.len());
        println!("  Saved to: {:?}", path);

        if verbose {
            for item in &items {
                println!();
                println!("  [{}] {}", item.id, item.problem_summary);
                println!("       Priority: {:?}", item.priority);
                println!("       Level:    {:?}", item.level);
                println!("       Owner:    {}", item.owner);
                println!("       Failures: {}", item.impact_scope.failure_count);
                println!("       Action:   {}", item.suggested_action);
            }
        }

        return Ok(());
    }

    // ── List mode ────────────────────────────────────────────────────
    if !actions_dir.join("actions.json").exists() {
        println!("No action items found at {:?}", actions_dir.join("actions.json"));
        println!("  Use `syscity eval action-items --generate` to generate from badcases.");
        return Ok(());
    }

    let items = load_action_items(&actions_dir)?;
    println!("═══ Action Items ({}) ═══", items.len());

    for item in &items {
        println!(
            "  [{}] [{:?}] {:?} — {} (owner: {})",
            item.id, item.priority, item.level, item.problem_summary, item.owner
        );
        if verbose {
            println!("         Root cause: {}", item.root_cause);
            println!("         Evidence:   {}", item.evidence.join("; "));
            println!("         Action:     {}", item.suggested_action);
            println!("         Failures:   {}", item.impact_scope.failure_count);
        }
    }

    Ok(())
}

/// `eval feedback` — view feedback pipeline stats for eval/ops/model channels.
async fn cmd_feedback(dir: Option<PathBuf>, channel: Option<String>, verbose: bool) -> Result<()> {
    use crate::eval::action::load_action_items;
    use crate::eval::recycle::{
        extract_rca_results_from_badcases, load_all_badcase_records, BadcaseGovernance,
    };

    let evals_dir = dir.unwrap_or_else(eval::default_evals_dir);

    let channel = channel.as_deref().unwrap_or("eval");

    match channel {
        "eval" => {
            let badcases_dir = evals_dir.join("badcases");
            let records = load_all_badcase_records(&evals_dir);
            let action_items = load_action_items(&evals_dir.join("actions")).unwrap_or_default();
            let rca_results = extract_rca_results_from_badcases(&evals_dir).unwrap_or_default();

            println!("═══ Feedback Pipeline: Eval ═══");
            println!("  Badcase records:   {}", records.len());
            println!("  RCA results:       {}", rca_results.len());
            println!("  Action items:      {}", action_items.len());
            println!("  Badcases directory: {:?}", badcases_dir);

            if verbose {
                let governance = BadcaseGovernance::default();
                let active = governance.filter_expired(&records);
                let expired = records.len().saturating_sub(active.len());
                println!();
                println!("  Governance:");
                println!("    Active records: {}", active.len());
                println!("    Expired (>{} days): {}", governance.max_age_days, expired);
                println!("    Max duplicates: {}", governance.max_duplicate_inputs);

                if !rca_results.is_empty() {
                    println!();
                    println!("  RCA modules:");
                    let mut counts: std::collections::HashMap<String, usize> =
                        std::collections::HashMap::new();
                    for r in &rca_results {
                        *counts
                            .entry(format!("{:?}", r.responsibility_module))
                            .or_insert(0) += 1;
                    }
                    for (module, count) in &counts {
                        println!("    {}: {}", module, count);
                    }
                }

                if !action_items.is_empty() {
                    println!();
                    println!("  Recent action items:");
                    for item in action_items.iter().take(5) {
                        println!(
                            "    [{}] {} — owner: {}",
                            item.id, item.problem_summary, item.owner
                        );
                    }
                }
            }
        }
        "ops" => {
            println!("═══ Feedback Pipeline: Ops ═══");
            println!("  Online experience signals: not connected");
            println!("  Human takeover rate:       N/A");
            println!("  Repeat query rate:         N/A");
            println!("  Complaint rate:            N/A");
            println!();
            println!("  Wire `FeedbackCollector::update_online()` to populate.");
        }
        "model" => {
            println!("═══ Feedback Pipeline: Model ═══");
            println!("  Model quality signals: not connected");
            println!("  Task completion rate:   N/A");
            println!("  Order closure rate:     N/A");
            println!();
            println!("  Wire `FeedbackCollector::update_business()` to populate.");
        }
        _ => {
            eprintln!("❌ Unknown channel '{}'. Use: eval, ops, or model", channel);
        }
    }

    Ok(())
}

/// `eval review` — list and manage human review cases.
async fn cmd_review(
    dir: Option<PathBuf>,
    pending_only: bool,
    verbose: bool,
    mark_reviewed: Option<String>,
) -> Result<()> {
    use eval::human_review::{HumanReviewStore, ReviewStatus};

    let evals_dir = dir.unwrap_or_else(eval::default_evals_dir);
    let store = HumanReviewStore::new(&evals_dir);

    // ── Mark a case as reviewed ─────────────────────────────────────
    if let Some(filename) = mark_reviewed {
        let review_dir = evals_dir.join("review");
        let file_path = if filename.ends_with(".json") {
            review_dir.join(&filename)
        } else {
            review_dir.join(format!("{}.json", filename))
        };

        if !file_path.exists() {
            eprintln!("❌ Review case not found: {:?}", file_path);
            return Ok(());
        }

        store.update_status(&file_path, ReviewStatus::Reviewed)?;
        let case = HumanReviewStore::load_case(&file_path)?;
        println!("✓ Marked as reviewed: {} (trial #{})", case.task_id, case.trial_index);
        return Ok(());
    }

    // ── List cases ──────────────────────────────────────────────────
    let filter = if pending_only {
        Some(ReviewStatus::Pending)
    } else {
        None
    };

    let cases = store.list_cases(filter)?;
    let (pending, reviewed, skipped) = store.count_by_status()?;

    if cases.is_empty() {
        println!("No review cases found in {:?}", evals_dir.join("review"));
        println!("  Pending: {}, Reviewed: {}, Skipped: {}", pending, reviewed, skipped);
        return Ok(());
    }

    println!(
        "Review cases ({} pending / {} reviewed / {} skipped):",
        pending, reviewed, skipped
    );

    for path in &cases {
        match HumanReviewStore::load_case(path) {
            Ok(case) => {
                let file_name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                println!();
                println!("  File: {}", file_name);
                println!("  Task: {} (trial #{})", case.task_id, case.trial_index);
                println!("  Status: {:?}", case.status);
                println!("  Confidence: {:.2}", case.scoring_output.confidence);
                println!("  Layer: {:?}", case.scoring_output.screening_layer);

                if verbose {
                    println!("  Input: {}", case.input.chars().take(120).collect::<String>());
                    println!("  Response: {}", case.response.chars().take(200).collect::<String>());
                    println!("  Judgment: {}", case.scoring_output.judgment_basis);
                    if let Some(ref v) = case.human_verdict {
                        println!("  Human verdict: {}", v);
                    }
                    if let Some(ref c) = case.human_comment {
                        println!("  Human comment: {}", c);
                    }
                }
            }
            Err(e) => {
                println!("  {:?}: error loading: {}", path, e);
            }
        }
    }

    println!();
    println!("Use --mark-reviewed=<filename> to mark a case as reviewed.");
    println!("Use --verbose to see case details.");

    Ok(())
}

/// `eval calibrate` — run Critic calibration against known-answer cases.
async fn cmd_calibrate(
    dir: Option<PathBuf>,
    file: Option<PathBuf>,
    show_history: bool,
    check_drift: bool,
    selection: eval::standalone::ProviderSelection,
) -> Result<()> {
    use std::sync::Arc;

    use crate::eval::calibration;
    use crate::providers::resolver::resolve_provider;

    let eval::standalone::ProviderSelection {
        provider: provider_override,
        model,
        api_key,
        base_url,
    } = selection;
    let provider = provider_override;

    let evals_dir = dir.unwrap_or_else(eval::default_evals_dir);

    // ── History / drift mode (no Critic needed) ─────────────────────
    if show_history || check_drift {
        let history = calibration::load_calibration_history(&evals_dir)?;

        if history.is_empty() {
            println!("No calibration history found in {:?}", evals_dir.join("calibration"));
            return Ok(());
        }

        println!("Calibration history ({} runs):", history.len());
        for (i, r) in history.iter().enumerate() {
            println!(
                "  #{:<3} verdict_acc={:.1}% dim_acc={:.1}% ({} cases)",
                i + 1,
                r.verdict_accuracy * 100.0,
                r.avg_dimension_accuracy * 100.0,
                r.total_cases,
            );
        }

        if check_drift {
            if let Some(msg) = calibration::detect_drift(&history) {
                println!("\n⚠️  Drift detected: {}", msg);
            } else {
                println!("\n✓ No significant drift detected.");
            }
        }

        return Ok(());
    }

    // ── Load calibration cases ──────────────────────────────────────
    let cal_file = file.unwrap_or_else(|| evals_dir.join("calibration").join("default.yaml"));
    if !cal_file.exists() {
        eprintln!("❌ Calibration file not found: {:?}", cal_file);
        println!("   Create a calibration file at evals/calibration/default.yaml");
        return Ok(());
    }

    let cases = calibration::load_calibration_cases(&cal_file)?;
    if cases.is_empty() {
        println!("No calibration cases found in {:?}", cal_file);
        return Ok(());
    }

    println!("═══ Critic Calibration ═══");
    println!("  File:  {:?}", cal_file);
    println!("  Cases: {}", cases.len());

    // ── Resolve provider & create Critic ────────────────────────────
    let provider_type = provider.unwrap_or_else(|| "anthropic".to_string());
    let provider = match resolve_provider(&provider_type, api_key, base_url, model.clone(), None) {
        Ok(p) => p,
        Err(e) => {
            return Err(crate::error::SyscityError::Validation(format!(
                "Failed to create provider '{}': {}",
                provider_type, e
            )));
        }
    };

    let mut critic = crate::agent::reflection::critic::Critic::new(provider);
    if let Some(ref m) = model {
        critic = critic.with_model(m.clone());
    }

    // ── Run calibration ─────────────────────────────────────────────
    println!("  Running {} cases...", cases.len());
    let report = calibration::calibrate(Arc::new(critic), &cases).await;

    // ── Print results ───────────────────────────────────────────────
    println!();
    println!("  Verdict accuracy:      {:.1}%", report.verdict_accuracy * 100.0);
    println!("  Avg dim accuracy:      {:.1}%", report.avg_dimension_accuracy * 100.0);
    println!("  Avg score deviation:   {:.3}", report.avg_score_deviation);
    println!("  Matching verdicts:     {}/{}", report.matching_verdicts, report.total_cases);
    println!();

    for result in &report.per_case {
        let icon = if result.verdict_match { "✓" } else { "✗" };
        println!(
            "  {} {} — exp={}, act={}, dim_acc={:.0}%",
            icon,
            result.case_id,
            result.expected_verdict,
            result.actual_verdict,
            result.dimension_accuracy * 100.0,
        );
    }

    // ── Persist ─────────────────────────────────────────────────────
    match calibration::save_calibration_report(&evals_dir, &report) {
        Ok(path) => println!("\n✓ Report saved to {:?}", path),
        Err(e) => eprintln!("Warning: failed to save report: {}", e),
    }

    // ── Check drift against last run ────────────────────────────────
    let history = calibration::load_calibration_history(&evals_dir)?;
    if let Some(msg) = calibration::detect_drift(&history) {
        println!("\n⚠️  Drift detected: {}", msg);
    }

    Ok(())
}

/// Recursively collect all `.yaml` files under a directory and call `f` on
/// each.
fn collect_yaml_files<F>(
    dir: &std::path::Path,
    dirs: &mut Vec<std::path::PathBuf>,
    f: &mut F,
) -> std::io::Result<()>
where
    F: FnMut(&std::path::Path) -> std::result::Result<(), crate::error::SyscityError>,
{
    use std::io;
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path.clone());
            } else if path.extension().map(|e| e == "yaml").unwrap_or(false) {
                f(&path).map_err(|e| io::Error::other(e.to_string()))?;
            }
        }
    }
    while let Some(subdir) = dirs.pop() {
        for entry in std::fs::read_dir(&subdir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().map(|e| e == "yaml").unwrap_or(false) {
                f(&path).map_err(|e| io::Error::other(e.to_string()))?;
            }
        }
    }
    Ok(())
}
