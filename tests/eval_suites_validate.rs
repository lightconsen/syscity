//! Deterministic validation of the evals/ YAML assets.
//!
//! Runs in the standard `cargo test` CI step so a broken suite manifest or
//! task file fails fast before anyone burns LLM tokens on `eval run --full`.

use std::path::{Path, PathBuf};

fn evals_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("evals")
}

fn collect_yaml(dir: &Path, out: &mut Vec<PathBuf>) {
    if !dir.is_dir() {
        return;
    }
    for entry in std::fs::read_dir(dir).expect("read evals dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_yaml(&path, out);
        } else if path.extension().is_some_and(|e| e == "yaml" || e == "yml") {
            out.push(path);
        }
    }
}

/// Every suite manifest under evals/suites/ must load and resolve its task
/// references (registry.yaml is a category index, not a suite manifest).
///
/// A manifest that declares `generated_tasks: true` points at task files this
/// repository must not distribute (GAIA), so it is asserted to load only when
/// its importer has run on this machine. Skipping is not leniency about a
/// broken manifest: the manifest is still parsed, and the absence has to be
/// declared by the manifest itself.
#[test]
fn suite_manifests_load() {
    let mut files = Vec::new();
    collect_yaml(&evals_dir().join("suites"), &mut files);
    assert!(!files.is_empty(), "no suite manifests found");

    let mut skipped_generated = 0usize;
    for f in files {
        let stem = f.file_stem().unwrap().to_string_lossy().to_string();
        if stem == "registry" {
            let content = std::fs::read_to_string(&f).expect("read registry.yaml");
            assert!(content.contains("suites:"), "registry.yaml lost its category index shape");
            continue;
        }
        let generated = syscity::eval::suite_declares_generated_tasks(&f);
        let suite = match syscity::eval::load_suite(&f, &stem) {
            Ok(suite) => suite,
            // A declared-generated manifest whose task files are absent is the
            // expected state on a fresh clone and in CI. Only the absence is
            // excused: a generated manifest that is present but malformed must
            // still fail here, or the flag would turn the check off.
            Err(e) if generated && e.to_string().contains("No such file or directory") => {
                skipped_generated += 1;
                continue;
            }
            Err(e) => panic!("suite {} failed to load: {e}", f.display()),
        };
        // `tasks.is_empty()` is not asserted: badcases.yaml is a deliberate
        // placeholder whose tasks come from load_badcase_suite().
        assert!(suite.trials >= 1, "{}: trials must be >= 1", suite.id);
        assert!(
            (0.0..=1.0).contains(&suite.min_pass_rate),
            "{}: min_pass_rate must be within [0, 1]",
            suite.id
        );
    }
    // Purely informational: a generated suite that is missing is skipped, and
    // printing the count keeps that visible rather than silent.
    if skipped_generated > 0 {
        println!("skipped {skipped_generated} generated suite(s): task files not present");
    }
}

/// Every task file in the category directories must parse and contain tasks.
#[test]
fn task_files_load() {
    for sub in ["capability", "regression", "adversarial", "skills"] {
        let dir = evals_dir().join(sub);
        let mut files = Vec::new();
        collect_yaml(&dir, &mut files);
        assert!(!files.is_empty(), "no task files under {}", dir.display());

        for f in files {
            let loaded = syscity::eval::load_tasks(&f)
                .unwrap_or_else(|e| panic!("task file {} failed: {e}", f.display()));
            assert!(
                !loaded.tasks.is_empty(),
                "{}: parsed successfully but contains no tasks",
                f.display()
            );
        }
    }
}

/// The Critic calibration set must parse and contain known-answer cases.
#[test]
fn calibration_cases_load() {
    let path = evals_dir().join("calibration").join("default.yaml");
    let cases = syscity::eval::load_calibration_cases(&path)
        .unwrap_or_else(|e| panic!("calibration file failed: {e}"));
    assert!(!cases.is_empty(), "calibration set is empty");
}
