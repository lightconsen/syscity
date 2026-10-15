use std::path::Path;

use crate::gateway::*;

/// Persist a `GatewayConfig` to disk atomically.
///
/// Writes to a temporary file next to `config_path` and renames it into place
/// so readers never observe a partially-written config.
///
/// With `previous`, only the keys that actually changed between `previous` and
/// `config` are written, and the rest of the file — **including the comments
/// and layout the user put there** — is left alone. That matters because the
/// alternative is a whole-document re-serialization: it reorders keys, drops
/// every comment, and makes a one-key change look like a rewritten file. It is
/// reached by ordinary actions, not just deliberate edits — changing the theme,
/// switching permission mode, or approving a tool with "remember" all write
/// config.
///
/// `None` means "write the whole document", which is right for a caller that
/// has changed the file's *shape* rather than a value (the legacy-config
/// migration does exactly that) and for callers with no previous state to
/// compare against. A file that cannot be read or parsed falls back to it too:
/// losing comments is better than losing the write.
pub(crate) async fn persist_config_atomic(
    config: &GatewayConfig,
    config_path: &Path,
    previous: Option<&GatewayConfig>,
) -> Result<(), String> {
    let toml_str = match previous {
        Some(before) => match patched_document(config, before, config_path).await {
            Ok(text) => text,
            Err(e) => {
                tracing::debug!(error = %e, "rewriting the whole config instead of patching it");
                serialize(config)?
            }
        },
        None => serialize(config)?,
    };

    let tmp_path = config_path.with_extension("toml.tmp");
    tokio::fs::write(&tmp_path, toml_str)
        .await
        .map_err(|e| format!("Failed to write temporary config file: {}", e))?;
    tokio::fs::rename(&tmp_path, config_path)
        .await
        .map_err(|e| format!("Failed to atomically replace config file: {}", e))?;

    Ok(())
}

fn serialize(config: &GatewayConfig) -> Result<String, String> {
    toml::to_string_pretty(config).map_err(|e| format!("TOML serialization failed: {}", e))
}

/// The file with only the changed values written into it.
async fn patched_document(
    config: &GatewayConfig,
    before: &GatewayConfig,
    config_path: &Path,
) -> Result<String, String> {
    let text = tokio::fs::read_to_string(config_path)
        .await
        .map_err(|e| format!("config file unreadable: {}", e))?;
    let mut document: toml_edit::Document = text.parse().map_err(|e| format!("{e}"))?;

    let before = toml::Value::try_from(before).map_err(|e| format!("{e}"))?;
    let after = toml::Value::try_from(config).map_err(|e| format!("{e}"))?;

    let mut changes = Vec::new();
    collect_changes(&before, &after, &mut Vec::new(), &mut changes);
    for (path, value) in changes {
        apply_change(document.as_table_mut(), &path, value.as_ref());
    }
    Ok(document.to_string())
}

/// One change to make: a dotted path, and the value to put there (`None` = the
/// key should go away).
type Change = (Vec<String>, Option<toml::Value>);

/// Walk two documents and record every leaf that differs.
///
/// A table on both sides is descended into, so a single changed key produces a
/// single change rather than a rewritten subtree. Anything else that differs —
/// a different type, an array with a different element, a key only one side has
/// — is replaced or inserted or removed wholesale.
fn collect_changes(
    before: &toml::Value,
    after: &toml::Value,
    path: &mut Vec<String>,
    out: &mut Vec<Change>,
) {
    match (before, after) {
        (toml::Value::Table(b), toml::Value::Table(a)) => {
            for key in b.keys().chain(a.keys().filter(|k| !b.contains_key(*k))) {
                path.push(key.clone());
                match (b.get(key), a.get(key)) {
                    (Some(bv), Some(av)) => collect_changes(bv, av, path, out),
                    // Only in the new document.
                    (None, Some(av)) => out.push((path.clone(), Some(av.clone()))),
                    // Only in the old one: the key was removed.
                    (Some(_), None) => out.push((path.clone(), None)),
                    (None, None) => {}
                }
                path.pop();
            }
        }
        (b, a) if b != a => out.push((path.clone(), Some(a.clone()))),
        _ => {}
    }
}

/// Write one change into the document, leaving everything else alone.
fn apply_change(table: &mut toml_edit::Table, path: &[String], value: Option<&toml::Value>) {
    let Some((key, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        match value {
            Some(value) => {
                // Assigning to an existing key keeps that key's own decor — the
                // comments above it and the spacing around it. A key that is
                // not there yet has no decor to keep, so it is inserted.
                let edit = || toml_edit::Item::Value(edit_value(value));
                match table.get_mut(key) {
                    Some(slot) => *slot = edit(),
                    None => {
                        table.insert(key, edit());
                    }
                }
            }
            None => {
                table.remove(key);
            }
        }
        return;
    }
    // Descend, creating the table only when the value is not a removal.
    if !table.contains_key(key) {
        if value.is_none() {
            return;
        }
        table.insert(key, toml_edit::Item::Table(toml_edit::Table::new()));
    }
    if let Some(child) = table.get_mut(key).and_then(|item| item.as_table_mut()) {
        apply_change(child, rest, value);
    }
}

/// A `toml::Value` as the editable kind.
fn edit_value(value: &toml::Value) -> toml_edit::Value {
    use toml_edit::Value as Edit;
    match value {
        toml::Value::String(s) => Edit::from(s.clone()),
        toml::Value::Integer(i) => Edit::from(*i),
        toml::Value::Float(f) => Edit::from(*f),
        toml::Value::Boolean(b) => Edit::from(*b),
        toml::Value::Datetime(d) => Edit::from(d.to_string()),
        toml::Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(edit_value(item));
            }
            Edit::Array(array)
        }
        toml::Value::Table(map) => {
            let mut inline = toml_edit::InlineTable::new();
            for (k, v) in map {
                inline.insert(k, edit_value(v));
            }
            Edit::InlineTable(inline)
        }
    }
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
        persist_config_atomic(&cfg, &path, None)
            .await
            .expect("persist");
        let on_disk = tokio::fs::read_to_string(&path).await.expect("read");
        assert!(on_disk.contains("model_provider"), "TOML should round-trip");
        let reparsed: GatewayConfig = toml::from_str(&on_disk).expect("reparse");
        assert_eq!(reparsed.model, cfg.model);
    }

    /// A config file someone has annotated keeps its annotations when a single
    /// setting changes. This is the whole point of the patch path: the
    /// alternative reformats the file and drops every comment.
    #[tokio::test]
    async fn a_one_key_change_keeps_the_comments_around_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        // A hand-written file, which is what this is protecting.
        let annotated = "# my syscity config — do not reformat\n\
                         [tui]\n\
                         # light palette in this terminal\n\
                         theme = \"auto\"\n\
                         \n\
                         # raised in June, watch memory\n\
                         port = 18080\n";
        tokio::fs::write(&path, annotated).await.expect("write");

        let before: GatewayConfig = toml::from_str(annotated).expect("fixture parses");
        let mut after = before.clone();
        after.tui.theme = crate::gateway::ThemeSetting::Light;
        persist_config_atomic(&after, &path, Some(&before))
            .await
            .expect("patch");

        let patched = tokio::fs::read_to_string(&path).await.expect("read back");
        assert!(patched.contains("# my syscity config"), "header comment gone:\n{patched}");
        assert!(
            patched.contains("# light palette in this terminal"),
            "the comment above the changed key gone:\n{patched}"
        );
        assert!(patched.contains("# raised in June"), "another key's comment gone:\n{patched}");
        assert!(patched.contains("theme = \"light\""), "the change did not land:\n{patched}");
        assert_eq!(patched.matches("[tui]").count(), 1, "no duplicated table:\n{patched}");

        let reparsed: GatewayConfig = toml::from_str(&patched).expect("reparses");
        assert_eq!(reparsed.tui.theme, crate::gateway::ThemeSetting::Light);
        assert_eq!(reparsed.port, 18080, "the untouched neighbour survived");
    }

    /// Nothing changed means nothing written: a persist of an identical config
    /// must leave the file byte-for-byte alone, including its layout.
    #[tokio::test]
    async fn an_unchanged_config_leaves_the_file_byte_identical() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        let cfg = GatewayConfig::default();
        persist_config_atomic(&cfg, &path, None)
            .await
            .expect("first write");

        let hand_written = "# keep me exactly as I am\nmodel_provider = \"openai\"\n";
        tokio::fs::write(&path, hand_written).await.expect("write");

        persist_config_atomic(&cfg, &path, Some(&cfg))
            .await
            .expect("no-op patch");
        let after = tokio::fs::read_to_string(&path).await.expect("read back");
        assert_eq!(after, hand_written, "an unchanged persist rewrote the file");
    }

    /// A key added since the file was written has to reach it, and a removed
    /// one has to leave — the patch is a patch of the whole document, not just
    /// the keys that happen to be present.
    #[tokio::test]
    async fn added_and_removed_keys_are_written() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        let before = GatewayConfig::default();
        persist_config_atomic(&before, &path, None)
            .await
            .expect("first write");

        let mut after = before.clone();
        after.security.fence_network = true;
        after
            .agent_models
            .insert("worker".into(), "some-model".into());
        persist_config_atomic(&after, &path, Some(&before))
            .await
            .expect("patch");

        let text = tokio::fs::read_to_string(&path).await.expect("read");
        let reparsed: GatewayConfig = toml::from_str(&text).expect("reparses");
        assert!(reparsed.security.fence_network, "an added key must land:\n{text}");
        assert_eq!(
            reparsed.agent_models.get("worker").map(String::as_str),
            Some("some-model"),
            "a key added to a table must land:\n{text}"
        );

        let mut removed = after.clone();
        removed.agent_models.remove("worker");
        persist_config_atomic(&removed, &path, Some(&after))
            .await
            .expect("patch");
        let text = tokio::fs::read_to_string(&path).await.expect("read");
        let reparsed: GatewayConfig = toml::from_str(&text).expect("reparses");
        assert!(reparsed.agent_models.get("worker").is_none(), "a removed key must go:\n{text}");
    }

    /// No file to patch means no reason to fail: the write still happens.
    #[tokio::test]
    async fn a_missing_file_falls_back_to_writing_the_whole_document() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        let cfg = GatewayConfig::default();
        persist_config_atomic(&cfg, &path, Some(&cfg))
            .await
            .expect("persist");
        let text = tokio::fs::read_to_string(&path).await.expect("read");
        assert!(text.contains("model_provider"), "the fallback wrote the document");
    }

    #[tokio::test]
    async fn persist_config_atomic_errors_on_bad_dir() {
        let err = persist_config_atomic(
            &GatewayConfig::default(),
            std::path::Path::new("/nonexistent-dir-xyz/config.toml"),
            None,
        )
        .await;
        assert!(err.is_err(), "write into missing dir must fail");
    }
}
