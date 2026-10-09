//! Skill management commands for Syscity

use std::path::PathBuf;

use clap::Subcommand;
use serde_json::json;

use crate::cli::ws;
use crate::cli::OutputFormat;
use crate::error::{Result, SyscityError};
use crate::skills::{install_all, SkillFile};

#[derive(Debug, Subcommand)]
pub enum SkillCommands {
    /// List all available skills
    List {
        /// Show all skills including ineligible ones
        #[arg(short, long)]
        all: bool,
        /// Output format
        #[arg(short, long, value_enum, default_value = "table")]
        format: OutputFormat,
    },
    /// Show detailed information about a skill
    Info {
        /// Skill name
        name: String,
    },
    /// Install a skill from a directory or git repo (fetches locally)
    Install {
        /// Path to skill directory or git URL
        source: String,
        /// Skill name (optional, defaults to directory name)
        #[arg(short, long)]
        name: Option<String>,
    },
    /// Install a skill from the marketplace catalog (by id)
    CatalogInstall {
        /// Skill id as it appears in the catalog
        id: String,
    },
    /// Uninstall a skill
    Uninstall {
        /// Skill name
        name: String,
        /// Skip confirmation
        #[arg(short, long)]
        force: bool,
    },
    /// Enable a skill
    Enable {
        /// Skill name
        name: String,
    },
    /// Disable a skill
    Disable {
        /// Skill name
        name: String,
    },
    /// Install dependencies for a skill
    Setup {
        /// Skill name (if not provided, sets up all eligible skills)
        name: Option<String>,
    },
    /// Run a skill with the given input
    Run {
        /// Skill name/ID
        id: String,
        /// Input message for the skill
        #[arg(short, long)]
        input: String,
        /// Additional context (JSON)
        #[arg(short, long)]
        context: Option<String>,
    },
    /// Create a new skill template
    Init {
        /// Skill name
        name: String,
        /// Target directory (defaults to ./<name>-skill)
        #[arg(short, long)]
        path: Option<PathBuf>,
        /// Template to use
        #[arg(short, long, default_value = "basic")]
        template: String,
    },
    /// Show a skill's install record (version, source, pin, usage, history)
    Versions {
        /// Skill name/ID
        id: String,
    },
    /// Pin a skill to its current version (or unpin with --unpin)
    Pin {
        /// Skill name/ID
        id: String,
        /// Release the pin instead of setting it
        #[arg(long)]
        unpin: bool,
    },
    /// Roll a skill back to a version kept in its history
    Rollback {
        /// Skill name/ID
        id: String,
        /// Version to restore (see `skill versions`)
        version: String,
    },
}

/// Run skill commands (over WebSocket).
pub async fn run_skill_command(command: &SkillCommands) -> Result<()> {
    match command {
        SkillCommands::List { all, format } => {
            let fmt_str = match format {
                crate::cli::OutputFormat::Table => "table",
                crate::cli::OutputFormat::Json => "json",
                crate::cli::OutputFormat::Yaml => "yaml",
                crate::cli::OutputFormat::Plain => "plain",
            };
            let payload =
                ws::call("skills.list", json!({ "all": *all, "format": fmt_str })).await?;
            println!("{}", payload);
        }
        SkillCommands::Info { name } => {
            let payload = ws::call("skills.get", json!({ "name": name })).await?;
            println!("{}", payload);
        }
        SkillCommands::Install { source, name } => {
            install_skill_source(source, name.as_deref()).await?;
        }
        SkillCommands::CatalogInstall { id } => {
            // The marketplace is the catalog: the gateway downloads, verifies
            // and installs the skill in one call (type routing happens there).
            match ws::call("connectors.catalog_install", json!({ "id": id })).await {
                Ok(payload) => {
                    let version = payload["version"].as_str().unwrap_or("?");
                    println!("Skill '{}' v{version} installed.", id);
                }
                Err(e) => {
                    eprintln!("Failed to install skill: {e}");
                    return Err(e);
                }
            }
        }
        SkillCommands::Uninstall { name, force } => {
            if !force {
                println!("Uninstall skill '{}'? Use --force to confirm.", name);
                return Ok(());
            }
            match ws::call("skills.uninstall", json!({ "name": name })).await {
                Ok(_) => println!("Skill '{}' uninstalled", name),
                Err(e) => {
                    eprintln!("Failed to uninstall skill: {}", e);
                    return Err(e);
                }
            }
        }
        SkillCommands::Enable { name } => {
            match ws::call("skills.enable", json!({ "id": name })).await {
                Ok(_) => println!("Skill '{}' enabled", name),
                Err(e) => {
                    eprintln!("Failed to enable skill: {}", e);
                    return Err(e);
                }
            }
        }
        SkillCommands::Disable { name } => {
            match ws::call("skills.disable", json!({ "id": name })).await {
                Ok(_) => println!("Skill '{}' disabled", name),
                Err(e) => {
                    eprintln!("Failed to disable skill: {}", e);
                    return Err(e);
                }
            }
        }
        SkillCommands::Setup { name } => {
            setup_skill_deps(name.as_deref()).await?;
        }
        SkillCommands::Init { name, path, template } => {
            init_skill_template(name, path.as_deref(), template).await?;
        }
        SkillCommands::Versions { id } => {
            let payload = ws::call("skills.versions", json!({ "id": id })).await?;
            println!(
                "{} v{} {} (source: {})",
                payload["id"].as_str().unwrap_or(id),
                payload["version"].as_str().unwrap_or("?"),
                if payload["pinned"].as_bool().unwrap_or(false) {
                    "[pinned]"
                } else {
                    ""
                },
                payload["source"].as_str().unwrap_or("authored"),
            );
            if let Some(usage) = payload.get("usage") {
                println!(
                    "  used {}×, last at {}",
                    usage["count"].as_u64().unwrap_or(0),
                    usage["last_used"].as_str().unwrap_or("never"),
                );
            }
            let history = payload["history"].as_array().cloned().unwrap_or_default();
            if !history.is_empty() {
                println!(
                    "  history: {}",
                    history
                        .iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        SkillCommands::Pin { id, unpin } => {
            let pinned = !unpin;
            ws::call("skills.pin", json!({ "id": id, "pinned": pinned })).await?;
            println!(
                "Skill '{}' {}.",
                id,
                if pinned {
                    "pinned to its current version"
                } else {
                    "unpinned"
                }
            );
        }
        SkillCommands::Rollback { id, version } => {
            ws::call("skills.rollback", json!({ "id": id, "version": version })).await?;
            println!("Skill '{}' rolled back to {version}.", id);
        }
        SkillCommands::Run { id, input, context } => {
            let body = json!({
                "id": id,
                "input": input,
                "context": context.as_ref().and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok()),
            });
            match ws::call("skills.run", body).await {
                Ok(payload) => {
                    println!("Skill '{}' executed successfully", id);
                    if let Some(result) = payload.get("result").and_then(|r| r.as_str()) {
                        println!("\n{}", result);
                    }
                    if let Some(usage) = payload.get("usage") {
                        println!("\nUsage: {}", usage);
                    }
                }
                Err(e) => {
                    eprintln!("Failed to run skill: {}", e);
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

/// Install a skill from a local path or git URL.
///
/// Goes through the daemon (`skills.install_source`) rather than copying here:
/// the guard runs before anything lands, the install is recorded, and the
/// running daemon reloads instead of waiting on a filesystem watcher event.
async fn install_skill_source(source: &str, name: Option<&str>) -> Result<()> {
    let payload =
        ws::call("skills.install_source", json!({ "source": source, "name": name })).await?;
    println!(
        "Skill '{}' installed and loaded ({} skills).",
        payload["id"].as_str().unwrap_or(source),
        payload["skills_loaded"].as_u64().unwrap_or(0),
    );
    Ok(())
}

/// Run install specs for one skill (or all skills in the skills dir).
async fn setup_skill_deps(name: Option<&str>) -> Result<()> {
    let skills_dir = crate::dirs::skills_dir();

    let skill_dirs: Vec<PathBuf> = if let Some(n) = name {
        vec![skills_dir.join(n)]
    } else {
        // Collect all subdirectories of the skills dir
        let mut dirs = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&skills_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                    dirs.push(entry.path());
                }
            }
        }
        dirs
    };

    for dir in skill_dirs {
        let skill_md = dir.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }
        let content = tokio::fs::read_to_string(&skill_md)
            .await
            .map_err(|e| SyscityError::Internal(format!("Failed to read {:?}: {}", skill_md, e)))?;

        let skill_file = match SkillFile::parse(&content, skill_md.clone()) {
            Ok(sf) => sf,
            Err(e) => {
                eprintln!("Skipping {:?}: failed to parse SKILL.md: {}", dir, e);
                continue;
            }
        };

        let specs = &skill_file.frontmatter.install;
        if specs.is_empty() {
            println!("No install specs for skill '{}'", skill_file.frontmatter.name);
            continue;
        }

        println!(
            "Installing {} dep(s) for skill '{}'...",
            specs.len(),
            skill_file.frontmatter.name
        );
        let results = install_all(specs).await;
        for (spec, result) in results {
            println!("  {:?} -> {:?}", spec, result);
        }
    }
    Ok(())
}

/// Create a new SKILL.md template in the given directory.
async fn init_skill_template(
    name: &str,
    path: Option<&std::path::Path>,
    template: &str,
) -> Result<()> {
    let target_dir = if let Some(p) = path {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(format!("{}-skill", name))
    };

    tokio::fs::create_dir_all(&target_dir).await.map_err(|e| {
        SyscityError::Internal(format!("Failed to create directory {:?}: {}", target_dir, e))
    })?;

    let skill_md_content = match template {
        "basic" => format!(
            r#"---
name: {name}
version: "0.1.0"
description: "A brief description of {name}"
author: ""
triggers:
  - type: command
    pattern: "/{name}"
    user_invocable: true
install: []
requires:
  bins: []
---

# {name}

Describe what this skill does here.

## Usage

Describe how to use this skill.
"#,
            name = name
        ),
        _ => format!(
            r#"---
name: {name}
version: "0.1.0"
description: "A brief description of {name}"
author: ""
triggers: []
install: []
---

# {name}
"#,
            name = name
        ),
    };

    let skill_md_path = target_dir.join("SKILL.md");
    tokio::fs::write(&skill_md_path, skill_md_content)
        .await
        .map_err(|e| SyscityError::Internal(format!("Failed to write SKILL.md: {}", e)))?;

    println!("Created skill '{}' at {:?}", name, target_dir);
    println!("Edit {:?} to configure your skill.", skill_md_path);
    Ok(())
}
