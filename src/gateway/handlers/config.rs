use std::path::Path;

use crate::gateway::*;

/// Persist a `GatewayConfig` to disk atomically.
///
/// Writes to a temporary file next to `config_path` and renames it into place
/// so readers never observe a partially-written config.
pub(crate) async fn persist_config_atomic(
    config: &GatewayConfig,
    config_path: &Path,
) -> Result<(), String> {
    let toml_str =
        toml::to_string_pretty(config).map_err(|e| format!("TOML serialization failed: {}", e))?;

    let tmp_path = config_path.with_extension("toml.tmp");
    tokio::fs::write(&tmp_path, toml_str)
        .await
        .map_err(|e| format!("Failed to write temporary config file: {}", e))?;
    tokio::fs::rename(&tmp_path, config_path)
        .await
        .map_err(|e| format!("Failed to atomically replace config file: {}", e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::GatewayConfig;

    #[tokio::test]
    async fn persist_config_atomic_roundtrip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        let cfg = GatewayConfig::default();
        persist_config_atomic(&cfg, &path).await.expect("persist");
        let on_disk = tokio::fs::read_to_string(&path).await.expect("read");
        assert!(on_disk.contains("model_provider"), "TOML should round-trip");
        let reparsed: GatewayConfig = toml::from_str(&on_disk).expect("reparse");
        assert_eq!(reparsed.model, cfg.model);
    }

    #[tokio::test]
    async fn persist_config_atomic_errors_on_bad_dir() {
        let err = persist_config_atomic(
            &GatewayConfig::default(),
            std::path::Path::new("/nonexistent-dir-xyz/config.toml"),
        )
        .await;
        assert!(err.is_err(), "write into missing dir must fail");
    }
}
