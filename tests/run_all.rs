//! E2E eval 集成测试
//!
//! 加载 evals/ 目录下的 YAML 评测用例，通过 EvalHarness 执行并断言通过率。
//! 可按筛选器运行特定 suite：
//!
//! ```bash
//! cargo test --test run_all -- smoke        # CI 快速通道 — 加载验证
//! cargo test --test run_all -- regression   # 回归集 — 加载验证
//! cargo test --test run_all -- full         # 发布门禁 — 加载验证
//! cargo test --test run_all -- validate     # 所有 YAML 格式验证
//! ```

use std::path::Path;

use syscity::eval::{default_evals_dir, load_suite, load_tasks};

/// ── Helpers ─────────────────────────────────────────────────────────────────

fn evals_dir() -> &'static Path {
    // Integration tests run from the crate root, so CARGO_MANIFEST_DIR is set
    let dir = default_evals_dir();
    Box::leak(dir.into_boxed_path())
}

fn suite_path(name: &str) -> std::path::PathBuf {
    evals_dir().join("suites").join(format!("{}.yaml", name))
}

/// Recursively collect .yaml files from a directory (replaces walkdir).
fn collect_yaml_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(d) = dirs.pop() {
        if let Ok(entries) = std::fs::read_dir(&d) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    dirs.push(p);
                } else if p.extension().map(|e| e == "yaml").unwrap_or(false) {
                    files.push(p);
                }
            }
        }
    }
    files
}

/// ── Integration tests ─────────────────────────────────────────────────────

/// CI 快速通道 — 加载验证（无需 Agent）
#[tokio::test]
async fn eval_smoke_ci() {
    let path = suite_path("ci_smoke");
    assert!(path.exists(), "ci_smoke.yaml not found at {:?}", path);

    let suite = load_suite(&path, "ci_smoke").unwrap_or_else(|e| {
        panic!("Failed to load ci_smoke suite: {}", e);
    });

    println!("=== CI 快速通道 ===");
    println!("  Name:    {}", suite.name);
    println!("  Tasks:   {}", suite.tasks.len());
    println!("  Trials:  {}", suite.trials);
    println!("  Min pass: {:.0}%", suite.min_pass_rate * 100.0);

    assert!(!suite.tasks.is_empty(), "CI smoke must have at least one task");
    assert!(suite.trials >= 1, "CI smoke trials must be >= 1");

    // Print task summary
    for task in &suite.tasks {
        let conds = task.conditions.len();
        let has_criteria = task.criteria.is_some();
        println!("  • {}: {} conds, criteria={}", task.id, conds, has_criteria);
    }

    println!("PASS: CI smoke suite loaded: {} tasks", suite.tasks.len());
}

/// 回归评测集 — 加载验证
#[tokio::test]
async fn eval_regression() {
    let path = suite_path("registry");
    assert!(path.exists(), "registry.yaml not found");

    let suite = load_suite(&path, "regression").unwrap_or_else(|e| {
        panic!("Failed to load regression suite: {}", e);
    });

    println!("=== 回归评测集 ===");
    println!("  Tasks:  {}", suite.tasks.len());
    for task in &suite.tasks {
        println!("  • {}: {} conditions", task.id, task.conditions.len());
    }

    assert!(!suite.tasks.is_empty(), "Regression suite must have tasks");
    println!("PASS: Regression suite loaded: {} tasks", suite.tasks.len());
}

/// 对抗评测集 — 加载验证
#[tokio::test]
async fn eval_adversarial() {
    let path = suite_path("registry");
    assert!(path.exists(), "registry.yaml not found");

    let suite = load_suite(&path, "adversarial").unwrap_or_else(|e| {
        panic!("Failed to load adversarial suite: {}", e);
    });

    println!("=== 对抗评测集 ===");
    println!("  Tasks:  {}", suite.tasks.len());
    for task in &suite.tasks {
        println!("  • {}: {} conditions", task.id, task.conditions.len());
    }

    assert!(!suite.tasks.is_empty(), "Adversarial suite must have tasks");
    println!("PASS: Adversarial suite loaded: {} tasks", suite.tasks.len());
}

/// 能力评测集 — 加载验证
#[tokio::test]
async fn eval_capability() {
    let path = suite_path("registry");
    assert!(path.exists(), "registry.yaml not found");

    let suite = load_suite(&path, "capability").unwrap_or_else(|e| {
        panic!("Failed to load capability suite: {}", e);
    });

    println!("=== 能力评测集 ===");
    println!("  Tasks:  {}", suite.tasks.len());
    for task in &suite.tasks {
        println!("  • {}: {} conditions", task.id, task.conditions.len());
    }

    assert!(!suite.tasks.is_empty(), "Capability suite must have tasks");
    println!("PASS: Capability suite loaded: {} tasks", suite.tasks.len());
}

/// 发布门禁 — 加载验证（includes registry sections）
#[tokio::test]
async fn eval_release_gate() {
    let path = suite_path("release_gate");
    assert!(path.exists(), "release_gate.yaml not found");

    let suite = load_suite(&path, "release_gate").unwrap_or_else(|e| {
        panic!("Failed to load release gate suite: {}", e);
    });

    println!("=== 发布门禁评估 ===");
    println!("  Tasks:  {}", suite.tasks.len());
    for task in &suite.tasks {
        println!("  • {}", task.id);
    }

    assert!(!suite.tasks.is_empty(), "Release gate must have tasks");
    println!("PASS: Release gate loaded: {} tasks", suite.tasks.len());
}

/// 验证所有 YAML 文件格式正确
#[tokio::test]
async fn eval_validate_all_yaml() {
    let evals_dir = evals_dir();
    let files = collect_yaml_files(evals_dir);
    let mut validated = 0;
    let mut errors = Vec::new();

    for path in &files {
        let is_suite = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n == "suites" || n == "calibration")
            .unwrap_or(false);

        if is_suite {
            // Validate suite manifest
            match std::fs::read_to_string(path) {
                Ok(content) => match serde_norway::from_str::<serde_norway::Value>(&content) {
                    Ok(_) => validated += 1,
                    Err(e) => errors.push(format!("{:?}: {}", path, e)),
                },
                Err(e) => errors.push(format!("{:?}: {}", path, e)),
            }
        } else {
            // Validate task file via load_tasks
            match load_tasks(path) {
                Ok(tasks) => {
                    validated += 1;
                    let name = path.file_name().unwrap().to_string_lossy();
                    println!("  ✓ {} ({} tasks)", name, tasks.tasks.len());
                }
                Err(e) => errors.push(format!("{:?}: {}", path, e)),
            }
        }
    }

    for err in &errors {
        eprintln!("YAML ERROR: {}", err);
    }
    assert!(
        errors.is_empty(),
        "{}/{} YAML files have parse errors",
        errors.len(),
        files.len()
    );
    println!("=== 格式验证: {} YAML files valid ===", validated);
}
