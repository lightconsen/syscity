//! Plugin management commands for Syscity

use std::path::PathBuf;

use clap::Subcommand;
use serde_json::json;

use crate::cli::ws;
use crate::error::{Result, SyscityError};

#[derive(Debug, Subcommand)]
pub enum PluginCommands {
    /// List all plugins
    List {
        /// Show loaded plugins only
        #[arg(short, long)]
        loaded: bool,
        /// Show verbose information
        #[arg(short, long)]
        verbose: bool,
    },
    /// Install a plugin from a directory (copies it to the plugins folder)
    Install {
        /// Path to plugin directory containing plugin.json
        path: PathBuf,
        /// Plugin name (optional, defaults to directory name)
        #[arg(short, long)]
        name: Option<String>,
    },
    /// Uninstall a plugin
    Uninstall {
        /// Plugin ID
        name: String,
        /// Skip confirmation
        #[arg(short, long)]
        force: bool,
    },
    /// Enable a plugin
    Enable {
        /// Plugin ID
        name: String,
    },
    /// Disable a plugin
    Disable {
        /// Plugin ID
        name: String,
    },
    /// Show plugin information
    Info {
        /// Plugin ID
        name: String,
    },
    /// Reload plugins (lists current state; full reload requires daemon
    /// restart)
    Reload,
    /// Install a plugin from the marketplace catalog (by id)
    CatalogInstall {
        /// Plugin id as it appears in the catalog
        id: String,
    },
    /// Search the marketplace catalog (client-side filter, like the UI)
    Search {
        /// Search query
        query: String,
    },
    /// Sign a plugin manifest with an ed25519 key
    Sign {
        /// Plugin directory name (within plugins directory)
        name: String,
        /// Path to file containing base64-encoded ed25519 secret key
        #[arg(short, long)]
        key_file: PathBuf,
    },
}

/// Run plugin commands (over WebSocket).
pub async fn run_plugin_command(command: &PluginCommands) -> Result<()> {
    match command {
        PluginCommands::List { loaded: _, verbose } => {
            let payload = ws::call("plugins.list", json!({})).await?;
            if *verbose {
                println!("{}", payload);
            } else {
                let empty = vec![];
                let plugins = payload["plugins"].as_array().unwrap_or(&empty);
                println!("Plugins ({}):", plugins.len());
                for p in plugins {
                    let id = p["id"].as_str().unwrap_or("?");
                    let name = p["name"].as_str().unwrap_or("?");
                    let enabled = p["enabled"].as_bool().unwrap_or(false);
                    let status = if enabled { "enabled" } else { "disabled" };
                    println!("  {} ({}) [{}]", name, id, status);
                }
            }
        }

        PluginCommands::Install { path, name } => {
            // Resolve destination inside the Syscity plugins directory.
            let plugins_dir = crate::dirs::config_dir().join("plugins");
            tokio::fs::create_dir_all(&plugins_dir)
                .await
                .map_err(|e| SyscityError::Internal(format!("Cannot create plugins dir: {}", e)))?;

            let dest_name = name
                .clone()
                .or_else(|| {
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| "plugin".to_string());

            let dest = plugins_dir.join(&dest_name);

            // Copy the plugin directory (or single file) into the plugins folder.
            if path.is_dir() {
                copy_dir_all(path, &dest).await?;
            } else {
                tokio::fs::copy(path, &dest)
                    .await
                    .map_err(|e| SyscityError::Internal(format!("Copy failed: {}", e)))?;
            }

            println!("Plugin installed to {:?}", dest);
            // Load it now instead of waiting for the next restart:
            // `plugins.reload_all` re-scans the plugins directory, so the
            // directory just copied is discovered and loaded. A failure here is
            // not fatal — the files are in place and the next start picks them
            // up — so it only tells the user what still has to happen.
            match ws::call("plugins.reload_all", json!({})).await {
                Ok(_) => println!("Loaded."),
                Err(e) => println!(
                    "Not loaded yet ({e}). The daemon will pick it up on its next start \
                     (or run `syscity plugin reload`)."
                ),
            }
        }

        PluginCommands::Uninstall { name, force } => {
            if !force {
                println!("Uninstall plugin '{}'? Use --force to confirm.", name);
                return Ok(());
            }
            match ws::call("plugins.unload", json!({ "id": name })).await {
                Ok(_) => {
                    println!("Plugin '{}' unloaded from daemon.", name);
                    println!(
                        "To remove files, delete {:?}",
                        crate::dirs::config_dir().join("plugins").join(name)
                    );
                }
                Err(e) => {
                    eprintln!("Failed to unload plugin: {}", e);
                    return Err(e);
                }
            }
        }

        PluginCommands::Enable { name } => {
            match ws::call("plugins.enable", json!({ "id": name })).await {
                Ok(_) => println!("Plugin '{}' enabled.", name),
                Err(e) => {
                    eprintln!("Failed to enable plugin: {}", e);
                    return Err(e);
                }
            }
        }

        PluginCommands::Disable { name } => {
            match ws::call("plugins.disable", json!({ "id": name })).await {
                Ok(_) => println!("Plugin '{}' disabled.", name),
                Err(e) => {
                    eprintln!("Failed to disable plugin: {}", e);
                    return Err(e);
                }
            }
        }

        PluginCommands::Info { name } => {
            let payload = ws::call("plugins.list", json!({})).await?;
            let empty = vec![];
            let plugins = payload["plugins"].as_array().unwrap_or(&empty);
            let found = plugins.iter().find(|p| {
                p["id"].as_str() == Some(name.as_str()) || p["name"].as_str() == Some(name.as_str())
            });
            match found {
                Some(p) => println!("{}", serde_json::to_string_pretty(p).unwrap_or_default()),
                None => eprintln!("Plugin '{}' not found", name),
            }
        }

        PluginCommands::Reload => match ws::call("plugins.reload_all", json!({})).await {
            Ok(payload) => {
                println!("Plugins reloaded successfully.");
                if payload.is_object() && !payload["message"].is_null() {
                    println!("{}", payload["message"].as_str().unwrap_or(""));
                }
            }
            Err(e) => {
                eprintln!("Reload failed: {}", e);
                return Err(e);
            }
        },

        PluginCommands::CatalogInstall { id } => {
            // The marketplace is the catalog: the gateway downloads, verifies
            // and loads the plugin in one call (route by `type` happens there).
            match ws::call("connectors.catalog_install", json!({ "id": id })).await {
                Ok(payload) => {
                    let version = payload["version"].as_str().unwrap_or("?");
                    let plugin_id = payload["plugin_id"].as_str().unwrap_or(id);
                    println!("Plugin '{}' v{version} installed and loaded ({}).", id, plugin_id);
                }
                Err(e) => {
                    eprintln!("Failed to install plugin: {e}");
                    return Err(e);
                }
            }
        }

        PluginCommands::Search { query } => {
            // No server-side search exists: `connectors.catalog` hands over the
            // whole document and the UI filters it locally. Same here, so the
            // two surfaces agree on what "search" means.
            match ws::call("connectors.catalog", json!({})).await {
                Ok(payload) => {
                    let query = query.to_lowercase();
                    let entries = payload["entries"].as_array().cloned().unwrap_or_default();
                    let matches: Vec<_> = entries
                        .iter()
                        .filter(|e| {
                            ["id", "display_name", "description"].iter().any(|key| {
                                e[*key]
                                    .as_str()
                                    .is_some_and(|v| v.to_lowercase().contains(&query))
                            })
                        })
                        .collect();
                    println!("Catalog matches for '{query}' ({}):", matches.len());
                    for e in matches {
                        println!(
                            "  {} [{}] v{} — {}",
                            e["display_name"].as_str().unwrap_or("?"),
                            e["type"].as_str().unwrap_or("?"),
                            e["version"].as_str().unwrap_or("?"),
                            e["id"].as_str().unwrap_or("?"),
                        );
                    }
                }
                Err(e) => {
                    eprintln!("Search failed: {e}");
                    return Err(e);
                }
            }
        }

        PluginCommands::Sign { name, key_file } => {
            let key_content = match tokio::fs::read_to_string(key_file).await {
                Ok(c) => c.trim().to_string(),
                Err(e) => {
                    eprintln!("Failed to read key file {:?}: {}", key_file, e);
                    return Err(SyscityError::Internal(format!(
                        "Cannot read key file {:?}: {}",
                        key_file, e
                    )));
                }
            };
            let body = json!({ "name": name, "secret_key": key_content });
            match ws::call("plugins.sign", body).await {
                Ok(payload) => {
                    println!("Plugin '{}' signed successfully.", name);
                    if !payload.is_null() && !payload["message"].is_null() {
                        println!("{}", payload["message"].as_str().unwrap_or(""));
                    }
                }
                Err(e) => {
                    eprintln!("Failed to sign plugin: {}", e);
                    return Err(e);
                }
            }
        }
    }

    Ok(())
}

/// Recursively copy a directory tree.
async fn copy_dir_all(src: &PathBuf, dst: &PathBuf) -> Result<()> {
    tokio::fs::create_dir_all(dst)
        .await
        .map_err(|e| SyscityError::Internal(format!("mkdir {:?}: {}", dst, e)))?;

    let mut entries = tokio::fs::read_dir(src)
        .await
        .map_err(|e| SyscityError::Internal(format!("read_dir {:?}: {}", src, e)))?;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            Box::pin(copy_dir_all(&src_path, &dst_path)).await?;
        } else {
            tokio::fs::copy(&src_path, &dst_path)
                .await
                .map_err(|e| SyscityError::Internal(format!("copy {:?}: {}", src_path, e)))?;
        }
    }

    Ok(())
}
