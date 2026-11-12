//! The repository's own gates must keep working, and must keep agreeing.
//!
//! Three checks in `scripts/` encode rules that a *future* edit can quietly
//! break without any Rust code changing:
//!
//! * `check-toolchain-parity.sh` — every script runs the toolchain CI declares
//!   (it once said `cargo +stable`, which resolved to a newer release than
//!   CI's pin and made the local CI mirror the least trustworthy gate here).
//! * `check-disk-space.sh` — a build-heavy check has the room to run, instead
//!   of dying in the linker with `No space left on device` and being reported
//!   as a compile error in unrelated modules.
//! * `staged-checks.sh` / `gate-integrity.sh` — still wired into the hook.
//!
//! These are cheap (no build, no network) and they fail for the same reason the
//! gates they check would: a declaration drifted.

// Panicking is the assertion mechanism in this file; the repo's precedent for
// test targets (`tests/tui_pty.rs`) is a file-level allow.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn script(name: &str) -> PathBuf {
    repo_root().join("scripts").join(name)
}

/// Run a gate script and return (success, combined output).
fn run(name: &str, args: &[&str]) -> (bool, String) {
    let path = script(name);
    assert!(path.is_file(), "{name} is missing — the gate itself was deleted");
    let out = Command::new(&path)
        .args(args)
        .current_dir(repo_root())
        .output()
        .unwrap_or_else(|e| panic!("failed to run {name}: {e}"));
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), combined)
}

/// The toolchain every script runs is the one the project declares.
///
/// This is the check that would have caught `cargo +stable` in
/// `scripts/ci-check.sh`: on a developer machine `+stable` is *not* necessarily
/// the pinned channel, so the mirror lints (and fails) on code CI accepts.
#[test]
fn toolchain_declarations_agree_everywhere() {
    let (ok, output) = run("check-toolchain-parity.sh", &[]);
    assert!(
        ok,
        "toolchain declarations disagree (rust-toolchain.toml, Cargo.toml, \
         .github/workflows/ci.yml, scripts/):\n{output}"
    );

    // The pinned channel must also be the one Cargo.toml calls the MSRV — the
    // parity script asserts it, this re-asserts the two files directly so a
    // bug in the script cannot hide the disagreement.
    let pinned_path = repo_root().join("rust-toolchain.toml");
    let Ok(pinned) = std::fs::read_to_string(&pinned_path) else {
        panic!("cannot read {}", pinned_path.display());
    };
    let channel = pinned
        .lines()
        .find_map(|l| l.trim().strip_prefix("channel"))
        .and_then(|l| l.split('"').nth(1));
    let Some(channel) = channel else {
        panic!("rust-toolchain.toml has no channel = \"...\" line");
    };

    let Ok(manifest) = std::fs::read_to_string(repo_root().join("Cargo.toml")) else {
        panic!("cannot read Cargo.toml");
    };
    assert!(
        manifest.contains(&format!("rust-version = \"{channel}\"")),
        "Cargo.toml rust-version does not match the pinned channel {channel}"
    );
}

/// The disk preflight reports both ways, so it can never become a no-op.
///
/// A gate that always passes is worse than no gate: it reads as coverage. Feed
/// it an impossible requirement and require a failure, then the real one and
/// require a pass.
#[test]
fn disk_preflight_can_fail_and_can_pass() {
    let (ok, output) = run("check-disk-space.sh", &["--min-gb", "1000000"]);
    assert!(!ok, "an impossible free-space requirement must fail, got success:\n{output}");
    assert!(
        output.contains("No space left on device"),
        "the failure must explain what a full disk looks like:\n{output}"
    );

    let (ok, output) = run("check-disk-space.sh", &["--min-gb", "0"]);
    assert!(ok, "zero required space must pass, got failure:\n{output}");
}

/// The pre-commit hook still runs the gates that reject a commit.
///
/// A hook file that exists but no longer calls a gate looks identical to one
/// that does. Each gate named here is a rule the project chose to enforce at
/// commit time; deleting its call removes the enforcement with no other signal.
#[test]
fn pre_commit_hook_runs_every_staged_gate() {
    let hook = repo_root().join(".githooks").join("pre-commit");
    let body = std::fs::read_to_string(&hook)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", hook.display()));

    for gate in ["staged-checks.sh", "gate-integrity.sh", "audit.sh"] {
        assert!(
            body.contains(gate),
            "the pre-commit hook no longer runs {gate}; commits would pass unguarded"
        );
    }

    // The hook must be the configured one, or none of the above runs at all.
    // `core.hooksPath` is accepted in either spelling: the documented
    // `git config core.hooksPath .githooks` (relative) and the absolute path
    // that same command produces when run with an explicit path.
    //
    // An unset value is *not* a failure: enabling the hook is per-clone and
    // documented in CLAUDE.md, and CI deliberately does not set it (see the
    // note in the `security` job) — CI is the backstop for a clone or a
    // `--no-verify` commit. What is a failure is pointing it somewhere else,
    // which silently disables every gate below.
    let configured = Command::new("git")
        .args(["config", "core.hooksPath"])
        .current_dir(repo_root())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    if !configured.is_empty() {
        let value = configured.trim_end_matches('/');
        let points_at_hooks = value == ".githooks"
            || std::fs::canonicalize(value)
                .ok()
                .zip(std::fs::canonicalize(repo_root().join(".githooks")).ok())
                .map(|(a, b)| a == b)
                .unwrap_or(false);
        assert!(
            points_at_hooks,
            "core.hooksPath is {configured:?}, which is not .githooks — the pre-commit \
             gates in this file would never run. Fix with: git config core.hooksPath .githooks"
        );
    }

    // Either way the hook must be executable, or git refuses to run it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = std::fs::metadata(&hook) else {
            panic!("cannot stat {}", hook.display());
        };
        assert!(
            meta.permissions().mode() & 0o111 != 0,
            "{} is not executable, so git will skip it",
            hook.display()
        );
    }
}

/// Every gate script the CI workflow calls actually exists.
///
/// CI referencing a missing or renamed script is a red job that looks like a
/// code failure; keeping the list honest costs one file read.
#[test]
fn every_script_ci_invokes_exists() {
    let workflow =
        std::fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).expect("read ci.yml");

    let mut missing = Vec::new();
    for line in workflow.lines() {
        // `./scripts/foo.sh`, optionally after `chmod +x`.
        let mut rest = line;
        while let Some(idx) = rest.find("./scripts/") {
            let tail = &rest[idx + 2..];
            let name: String = tail
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
                .collect();
            let name = name.trim_end_matches(['\\', ';']);
            if (name.ends_with(".sh") || name.ends_with(".py"))
                && !Path::new(&repo_root().join(name)).is_file()
            {
                missing.push(name.to_string());
            }
            rest = &rest[idx + 10..];
        }
    }
    assert!(missing.is_empty(), "ci.yml invokes scripts that do not exist: {missing:?}");
}
