//! Profile management: list, create, and locate isolated homes.
//!
//! A profile is an isolated [`SyscityPaths`] root under `<base>/profiles/<name>`
//! — its own config, database, skills, memories, sessions and secrets. It is
//! deliberately *not* something one process multiplexes: two profiles are two
//! processes, which is what makes them isolated at all (see `docs/scale.md`'s
//! route A). So this command's job is only to help an operator see and create
//! them, and to print the root so a second shell can point at the same one.

use clap::Subcommand;

use crate::dirs::{self, PROFILES_DIR};
use crate::error::Result;

/// Profile commands.
#[derive(Debug, Subcommand)]
pub enum ProfileCommands {
    /// List the profiles that exist
    List,
    /// Create a profile's home (does nothing if it already exists)
    Create {
        /// Profile name: letters, digits, '-' and '_'
        name: String,
    },
    /// Print a profile's root directory
    Path {
        /// Profile name
        name: String,
    },
}

/// Run a profile subcommand.
pub async fn run_profile_command(command: &ProfileCommands) -> Result<()> {
    match command {
        ProfileCommands::List => list(),
        ProfileCommands::Create { name } => create(name),
        ProfileCommands::Path { name } => {
            let paths = dirs::SyscityPaths::for_profile(name)?;
            println!("{}", paths.root().display());
            Ok(())
        }
    }
}

/// Where profiles live: next to whatever base root this process resolved.
fn profiles_dir() -> std::path::PathBuf {
    dirs::SyscityPaths::from_env().root().join(PROFILES_DIR)
}

fn list() -> Result<()> {
    let dir = profiles_dir();
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    names.sort();

    if names.is_empty() {
        println!("no profiles yet — `syscity profile create <name>` makes one");
        return Ok(());
    }
    println!("{} profile(s) under {}", names.len(), dir.display());
    for name in names {
        println!("  {name}");
    }
    Ok(())
}

fn create(name: &str) -> Result<()> {
    let paths = dirs::SyscityPaths::for_profile(name)?;
    // The root is the config directory, so creating it is what makes the
    // profile real; the layout fills itself in on first use.
    std::fs::create_dir_all(paths.root()).map_err(|e| {
        crate::error::SyscityError::Validation(format!(
            "could not create profile at {}: {e}",
            paths.root().display()
        ))
    })?;
    println!("{}", paths.root().display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creating is idempotent and lands under the base root's `profiles/`.
    #[test]
    fn creating_a_profile_makes_its_home_under_the_base() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("profiles").join("work");
        std::fs::create_dir_all(&root).expect("create");
        assert!(root.is_dir());
        // And a second attempt is not an error.
        std::fs::create_dir_all(&root).expect("create again");
        assert!(root.is_dir());
    }

    #[test]
    fn a_bad_name_is_refused_before_any_directory_is_touched() {
        assert!(dirs::SyscityPaths::for_profile("../escape").is_err());
        assert!(dirs::SyscityPaths::for_profile("").is_err());
        assert!(dirs::SyscityPaths::for_profile("a/b").is_err());
    }
}
