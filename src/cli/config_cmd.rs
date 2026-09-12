//! Configuration management commands for Syscity
//!
//! Get, set, unset, and validate configuration values directly.
//! Supports nested dot-notation paths (e.g., providers.deepseek.api_key).

use std::path::PathBuf;

use clap::Subcommand;

use crate::error::Result;

#[derive(Debug, Subcommand)]
pub enum ConfigCommands {
    /// Show the current configuration
    Show {
        /// Output format
        #[arg(short, long, value_enum, default_value = "toml")]
        format: super::ConfigFormat,
    },
    /// Get a specific configuration value by key (dot-notation, e.g.,
    /// gateway.host)
    Get {
        /// Configuration key path
        key: String,
    },
    /// Set a configuration value by key (format: key=value)
    Set {
        /// Configuration key=value pair
        key_value: String,
        /// Path to configuration file
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// Unset (remove) a configuration value by key
    Unset {
        /// Configuration key path
        key: String,
        /// Path to configuration file
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// Show the configuration file path
    File,
    /// Validate the current configuration
    Validate,
}

/// Run config commands
pub async fn run_config_command(command: &ConfigCommands) -> Result<()> {
    match command {
        ConfigCommands::Show { format } => show_config(format).await,
        ConfigCommands::Get { key } => get_config_value(key).await,
        ConfigCommands::Set { key_value, file } => set_config_value(key_value, file.as_ref()).await,
        ConfigCommands::Unset { key, file } => unset_config_value(key, file.as_ref()).await,
        ConfigCommands::File => show_config_file().await,
        ConfigCommands::Validate => validate_config().await,
    }
}

async fn show_config_file() -> Result<()> {
    let path = crate::dirs::default_config_file();
    println!("{}", path.display());
    Ok(())
}

async fn show_config(format: &super::ConfigFormat) -> Result<()> {
    let config_path = crate::dirs::default_config_file();

    if !config_path.exists() {
        println!("# No configuration file found at {:?}", config_path);
        return Ok(());
    }

    let content = tokio::fs::read_to_string(&config_path).await?;

    match format {
        super::ConfigFormat::Toml => {
            println!("{}", content);
        }
        super::ConfigFormat::Json => {
            let value: toml::Value = toml::from_str(&content)?;
            let json = serde_json::to_string_pretty(&value)?;
            println!("{}", json);
        }
        super::ConfigFormat::Yaml => {
            let value: toml::Value = toml::from_str(&content)?;
            let yaml = serde_norway::to_string(&value)?;
            println!("{}", yaml);
        }
    }

    Ok(())
}

async fn get_config_value(key: &str) -> Result<()> {
    let config_path = crate::dirs::default_config_file();

    if !config_path.exists() {
        eprintln!("Configuration file not found at {:?}", config_path);
        return Ok(());
    }

    let content = tokio::fs::read_to_string(&config_path).await?;
    let value: toml::Value = toml::from_str(&content)?;

    match get_value_at_path(&value, key) {
        Some(v) => println!("{}", format_toml_value(v)),
        None => eprintln!("Key '{}' not found in configuration", key),
    }

    Ok(())
}

pub(crate) async fn set_config_value(key_value: &str, file: Option<&PathBuf>) -> Result<()> {
    let parts: Vec<&str> = key_value.splitn(2, '=').collect();
    if parts.len() != 2 {
        eprintln!("Invalid format. Use: key=value");
        return Ok(());
    }

    let key = parts[0];
    let value = parts[1];

    let config_path = file
        .cloned()
        .unwrap_or_else(crate::dirs::default_config_file);

    let mut toml_value = if config_path.exists() {
        let content = tokio::fs::read_to_string(&config_path).await?;
        toml::from_str(&content)?
    } else {
        toml::Value::Table(toml::map::Map::new())
    };

    let parsed = parse_config_value(value);
    set_value_at_path(&mut toml_value, key, parsed)?;

    let new_content = toml::to_string_pretty(&toml_value)?;
    tokio::fs::write(&config_path, new_content).await?;
    println!("✅ Set {} = {} in {:?}", key, value, config_path);

    Ok(())
}

pub(crate) async fn unset_config_value(key: &str, file: Option<&PathBuf>) -> Result<()> {
    let config_path = file
        .cloned()
        .unwrap_or_else(crate::dirs::default_config_file);

    if !config_path.exists() {
        eprintln!("Configuration file not found at {:?}", config_path);
        return Ok(());
    }

    let content = tokio::fs::read_to_string(&config_path).await?;
    let mut toml_value: toml::Value = toml::from_str(&content)?;

    remove_value_at_path(&mut toml_value, key)?;

    let new_content = toml::to_string_pretty(&toml_value)?;
    tokio::fs::write(&config_path, new_content).await?;
    println!("✅ Unset {} in {:?}", key, config_path);

    Ok(())
}

async fn validate_config() -> Result<()> {
    use crate::config::Config;

    match Config::load() {
        Ok(_config) => {
            println!("✅ Configuration is valid");
            Ok(())
        }
        Err(e) => {
            eprintln!("❌ Configuration error: {}", e);
            Err(e)
        }
    }
}

fn format_toml_value(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => format!("\"{}\"", s),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(dt) => dt.to_string(),
        toml::Value::Array(_) | toml::Value::Table(_) => toml::to_string_pretty(value)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

// ── Path traversal utilities for toml::Value ────────────────────────────────

fn get_value_at_path<'a>(value: &'a toml::Value, path: &str) -> Option<&'a toml::Value> {
    let mut current = value;
    for part in path.split('.') {
        match current {
            toml::Value::Table(map) => {
                current = map.get(part)?;
            }
            toml::Value::Array(arr) => {
                let index = part.parse::<usize>().ok()?;
                current = arr.get(index)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

fn set_value_at_path(value: &mut toml::Value, path: &str, new_value: toml::Value) -> Result<()> {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.is_empty() {
        return Err(crate::error::SyscityError::Validation("Empty config path".to_string()));
    }

    // Navigate to the parent of the target key
    let mut current = value;
    for part in &parts[..parts.len() - 1] {
        match current {
            toml::Value::Table(map) => {
                let next = map
                    .entry(part.to_string())
                    .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
                current = next;
            }
            _ => {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Cannot navigate through non-table value at '{}'",
                    part
                )));
            }
        }
    }

    // Insert at the final key
    let last = parts[parts.len() - 1];
    match current {
        toml::Value::Table(map) => {
            map.insert(last.to_string(), new_value);
        }
        _ => {
            return Err(crate::error::SyscityError::Validation(format!(
                "Cannot set key '{}' on non-table value",
                last
            )));
        }
    }

    Ok(())
}

fn remove_value_at_path(value: &mut toml::Value, path: &str) -> Result<()> {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.is_empty() {
        return Err(crate::error::SyscityError::Validation("Empty config path".to_string()));
    }

    let mut current = value;
    for (i, part) in parts.iter().enumerate() {
        let is_last = i == parts.len() - 1;

        if is_last {
            match current {
                toml::Value::Table(map) => {
                    if map.remove(*part).is_none() {
                        return Err(crate::error::SyscityError::Validation(format!(
                            "Key '{}' not found",
                            path
                        )));
                    }
                }
                _ => {
                    return Err(crate::error::SyscityError::Validation(format!(
                        "Cannot remove key '{}' from non-table value",
                        part
                    )));
                }
            }
        } else {
            match current {
                toml::Value::Table(map) => {
                    current = map.get_mut(*part).ok_or_else(|| {
                        crate::error::SyscityError::Validation(format!(
                            "Key '{}' not found in path",
                            part
                        ))
                    })?;
                }
                _ => {
                    return Err(crate::error::SyscityError::Validation(format!(
                        "Cannot navigate through non-table value at '{}'",
                        part
                    )));
                }
            }
        }
    }

    Ok(())
}

// ── Value parsing with type inference ───────────────────────────────────────

fn parse_config_value(input: &str) -> toml::Value {
    let trimmed = input.trim();

    // Boolean
    if trimmed.eq_ignore_ascii_case("true") {
        return toml::Value::Boolean(true);
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return toml::Value::Boolean(false);
    }

    // Integer
    if let Ok(n) = trimmed.parse::<i64>() {
        return toml::Value::Integer(n);
    }

    // Float
    if let Ok(n) = trimmed.parse::<f64>() {
        return toml::Value::Float(n);
    }

    // Array: [1, 2, 3] or ["a", "b"]
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(trimmed) {
            let toml_arr: Vec<toml::Value> = arr.into_iter().map(json_to_toml).collect();
            return toml::Value::Array(toml_arr);
        }
    }

    // Object: {"key": "value"}
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        if let Ok(obj) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(trimmed)
        {
            let mut map = toml::map::Map::new();
            for (k, v) in obj {
                map.insert(k, json_to_toml(v));
            }
            return toml::Value::Table(map);
        }
    }

    // DateTime: try ISO 8601 format
    if let Ok(dt) = trimmed.parse::<toml::value::Datetime>() {
        return toml::Value::Datetime(dt);
    }

    // Default: string
    toml::Value::String(trimmed.to_string())
}

fn json_to_toml(value: serde_json::Value) -> toml::Value {
    match value {
        serde_json::Value::Null => toml::Value::String(String::new()),
        serde_json::Value::Bool(b) => toml::Value::Boolean(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else {
                toml::Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => toml::Value::String(s),
        serde_json::Value::Array(arr) => {
            toml::Value::Array(arr.into_iter().map(json_to_toml).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut map = toml::map::Map::new();
            for (k, v) in obj {
                map.insert(k, json_to_toml(v));
            }
            toml::Value::Table(map)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bool() {
        assert_eq!(parse_config_value("true"), toml::Value::Boolean(true));
        assert_eq!(parse_config_value("false"), toml::Value::Boolean(false));
        assert_eq!(parse_config_value("TRUE"), toml::Value::Boolean(true));
    }

    #[test]
    fn test_parse_integer() {
        assert_eq!(parse_config_value("42"), toml::Value::Integer(42));
        assert_eq!(parse_config_value("-3"), toml::Value::Integer(-3));
    }

    #[test]
    fn test_parse_float() {
        assert_eq!(parse_config_value("3.14"), toml::Value::Float(3.14));
    }

    #[test]
    fn test_parse_string() {
        assert_eq!(parse_config_value("hello"), toml::Value::String("hello".to_string()));
    }

    #[test]
    fn test_parse_array() {
        let val = parse_config_value("[1, 2, 3]");
        assert_eq!(
            val,
            toml::Value::Array(vec![
                toml::Value::Integer(1),
                toml::Value::Integer(2),
                toml::Value::Integer(3),
            ])
        );
    }

    #[test]
    fn test_parse_object() {
        let val = parse_config_value(r#"{"key": "value"}"#);
        let mut map = toml::map::Map::new();
        map.insert("key".to_string(), toml::Value::String("value".to_string()));
        assert_eq!(val, toml::Value::Table(map));
    }

    #[test]
    fn test_get_value_at_path() {
        let mut map = toml::map::Map::new();
        let mut inner = toml::map::Map::new();
        inner.insert("api_key".to_string(), toml::Value::String("secret".to_string()));
        map.insert("providers".to_string(), toml::Value::Table(inner));

        let value = toml::Value::Table(map);
        assert_eq!(
            get_value_at_path(&value, "providers.api_key"),
            Some(&toml::Value::String("secret".to_string()))
        );
        assert_eq!(get_value_at_path(&value, "providers.missing"), None);
    }

    #[test]
    fn test_set_value_at_path_nested() {
        let mut value = toml::Value::Table(toml::map::Map::new());
        set_value_at_path(
            &mut value,
            "providers.deepseek.api_key",
            toml::Value::String("sk-test".to_string()),
        )
        .unwrap();

        assert_eq!(
            get_value_at_path(&value, "providers.deepseek.api_key"),
            Some(&toml::Value::String("sk-test".to_string()))
        );
    }

    #[test]
    fn test_remove_value_at_path() {
        let mut map = toml::map::Map::new();
        map.insert("key".to_string(), toml::Value::String("value".to_string()));
        let mut value = toml::Value::Table(map);

        remove_value_at_path(&mut value, "key").unwrap();
        assert_eq!(get_value_at_path(&value, "key"), None);
    }
}
