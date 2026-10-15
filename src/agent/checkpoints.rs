//! Session checkpoints: what a session's file writes are about to replace.
//!
//! A checkpoint cannot be reconstructed after the fact. The content a write
//! destroys is gone the moment it happens, so the only way to offer "put that
//! back" is to keep a copy *before* the write — which is why this is a hook on
//! the writing tools rather than a reader of the audit log.
//!
//! Nothing is recorded until a checkpoint exists. `/snapshot` opens one
//! (remembered on disk, so a restart does not quietly stop recording and leave
//! a later rollback half-done), every write into a path while one is open keeps
//! that path's pre-image once, and `/rollback` puts the files back and drops the
//! checkpoints from that point on — they describe a state that no longer
//! applies.
//!
//! What this deliberately does not cover: `shell` and anything else that writes
//! a file without naming it in a `path` argument. `docs/modules/tools.md` states
//! the project's position — `ToolCapabilities::compensation` lets a tool declare
//! its own undo, and there is no machinery pretending to reverse a side effect —
//! and this is the same position with one narrow exception: the three tools that
//! name the file they are about to overwrite.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A checkpoint on disk: the pre-images taken while it was open.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointInfo {
    /// Identifies the checkpoint to `/rollback`.
    pub id: String,
    /// What the user called it, if anything.
    pub label: Option<String>,
    /// When it was opened.
    pub created_at: DateTime<Utc>,
    /// How many files it holds a pre-image of.
    pub files: usize,
}

/// What a rollback did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    /// Files whose content was put back.
    pub restored: usize,
    /// Files that did not exist before the writes and were removed.
    pub removed: usize,
    /// Checkpoints dropped along with this one.
    pub dropped: usize,
}

/// The manifest of one checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    id: String,
    label: Option<String>,
    created_at: DateTime<Utc>,
    files: Vec<CapturedFile>,
}

/// One path's pre-image.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CapturedFile {
    /// Where the file was.
    path: PathBuf,
    /// Whether it existed at all — a write that creates a file is undone by
    /// removing it, not by restoring empty content.
    existed: bool,
    /// The copy's file name inside the checkpoint directory, when it existed.
    blob: Option<String>,
}

/// Which checkpoint a session is currently recording into.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Open {
    id: String,
}

/// Session checkpoints on disk.
#[derive(Debug)]
pub struct CheckpointStore {
    root: PathBuf,
}

impl CheckpointStore {
    /// A store rooted at `root` (`<home>/checkpoints`).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Open a checkpoint for `session`; writes from now on are recorded into it.
    ///
    /// Opening a second one supersedes the first, which is the honest reading:
    /// a checkpoint marks "the state as of here", and there is only one here.
    pub async fn begin(
        &self,
        session: &str,
        label: Option<String>,
    ) -> crate::Result<CheckpointInfo> {
        let dir = self.session_dir(session);
        let id = format!("{}-{}", Utc::now().format("%Y%m%d-%H%M%S"), short_id());
        let checkpoint_dir = dir.join(&id);
        tokio::fs::create_dir_all(&checkpoint_dir)
            .await
            .map_err(|e| storage_error(format!("create {}", checkpoint_dir.display()), e))?;

        let manifest = Manifest {
            id: id.clone(),
            label,
            created_at: Utc::now(),
            files: Vec::new(),
        };
        self.write_manifest(&dir, &manifest).await?;
        self.set_open(&dir, &id).await?;

        Ok(CheckpointInfo {
            id,
            label: manifest.label,
            created_at: manifest.created_at,
            files: 0,
        })
    }

    /// The checkpoint `session` is recording into, if any.
    pub async fn open(&self, session: &str) -> Option<String> {
        let path = self.session_dir(session).join("open.json");
        let text = tokio::fs::read_to_string(&path).await.ok()?;
        serde_json::from_str::<Open>(&text).ok().map(|open| open.id)
    }

    /// Keep `path`'s current content, the first time it is written into while a
    /// checkpoint is open.
    ///
    /// A no-op when nothing is open, and a no-op for a path already captured in
    /// this checkpoint: the pre-image is what the file looked like when the
    /// checkpoint was opened, not what the last write left behind.
    ///
    /// Failures are returned rather than swallowed — a checkpoint that silently
    /// missed a file would roll back to a state that never existed — but the
    /// caller is a tool about to run, so it decides what to do about it.
    pub async fn capture(&self, session: &str, path: &Path) -> crate::Result<bool> {
        let dir = self.session_dir(session);
        let Some(id) = self.open(session).await else {
            return Ok(false);
        };
        let mut manifest = self.read_manifest(&dir, &id).await?;
        if manifest.files.iter().any(|file| file.path == path) {
            return Ok(false);
        }

        let checkpoint_dir = dir.join(&id);
        let existed = tokio::fs::try_exists(path).await.unwrap_or(false);
        let blob = if existed {
            let name = format!("{}.blob", manifest.files.len());
            tokio::fs::copy(path, checkpoint_dir.join(&name))
                .await
                .map_err(|e| storage_error(format!("copy {}", path.display()), e))?;
            Some(name)
        } else {
            None
        };

        manifest.files.push(CapturedFile {
            path: path.to_path_buf(),
            existed,
            blob,
        });
        self.write_manifest(&dir, &manifest).await?;
        Ok(true)
    }

    /// Every checkpoint for a session, newest first.
    pub async fn list(&self, session: &str) -> Vec<CheckpointInfo> {
        let dir = self.session_dir(session);
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            return Vec::new();
        };

        let mut checkpoints = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if !entry.path().is_dir() {
                continue;
            }
            match self.read_manifest(&dir, &name).await {
                Ok(manifest) => checkpoints.push(CheckpointInfo {
                    id: manifest.id,
                    label: manifest.label,
                    created_at: manifest.created_at,
                    files: manifest.files.len(),
                }),
                Err(e) => {
                    tracing::warn!("Ignoring unreadable checkpoint {}: {}", name, e);
                }
            }
        }
        checkpoints.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        checkpoints
    }

    /// Put the files back as they were when `id` was opened, and drop `id` and
    /// everything newer.
    ///
    /// The newer ones go because they describe a state that no longer applies:
    /// rolling back to Monday and then rolling back to Tuesday would restore
    /// Tuesday's pre-images over Monday's files, which is neither state.
    pub async fn rollback(&self, session: &str, id: &str) -> crate::Result<RestoreReport> {
        let dir = self.session_dir(session);
        let manifest = self.read_manifest(&dir, id).await?;
        let checkpoint_dir = dir.join(id);

        let mut restored = 0;
        let mut removed = 0;
        for file in &manifest.files {
            match (&file.blob, file.existed) {
                (Some(blob), true) => {
                    if let Some(parent) = file.path.parent() {
                        tokio::fs::create_dir_all(parent).await.map_err(|e| {
                            storage_error(format!("create {}", parent.display()), e)
                        })?;
                    }
                    tokio::fs::copy(checkpoint_dir.join(blob), &file.path)
                        .await
                        .map_err(|e| {
                            storage_error(format!("restore {}", file.path.display()), e)
                        })?;
                    restored += 1;
                }
                (None, false) => {
                    // It did not exist before the writes: undoing the write
                    // means removing the file, not restoring it empty.
                    match tokio::fs::remove_file(&file.path).await {
                        Ok(()) => removed += 1,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(storage_error(format!("remove {}", file.path.display()), e))
                        }
                    }
                }
                _ => {
                    tracing::warn!(
                        "Checkpoint {} records {} with no pre-image; leaving it as it is",
                        id,
                        file.path.display()
                    );
                }
            }
        }

        let dropped = self.drop_from(&dir, manifest.created_at).await?;
        Ok(RestoreReport { restored, removed, dropped })
    }

    /// Remove `checkpoint` and every checkpoint newer than it, and stop
    /// recording (nothing older is a boundary worth keeping either — the files
    /// are now whatever the newest remaining checkpoint was taken against).
    async fn drop_from(
        &self,
        dir: &Path,
        checkpoint_created_at: DateTime<Utc>,
    ) -> crate::Result<usize> {
        let mut dropped = 0;
        for checkpoint in self.list_of(dir).await {
            if checkpoint.created_at >= checkpoint_created_at {
                let path = dir.join(&checkpoint.id);
                tokio::fs::remove_dir_all(&path)
                    .await
                    .map_err(|e| storage_error(format!("remove {}", path.display()), e))?;
                dropped += 1;
            }
        }
        // Recording stops with the rollback: a checkpoint still open would
        // capture pre-images of the state we just left.
        let open_path = dir.join("open.json");
        match tokio::fs::remove_file(&open_path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(storage_error(format!("remove {}", open_path.display()), e)),
        }
        Ok(dropped)
    }

    /// Every checkpoint in a directory, without the session lookup.
    async fn list_of(&self, dir: &Path) -> Vec<CheckpointInfo> {
        let mut checkpoints = Vec::new();
        let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
            return checkpoints;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if !entry.path().is_dir() {
                continue;
            }
            if let Ok(manifest) = self.read_manifest(dir, &name).await {
                checkpoints.push(CheckpointInfo {
                    id: manifest.id,
                    label: manifest.label,
                    created_at: manifest.created_at,
                    files: manifest.files.len(),
                });
            }
        }
        checkpoints
    }

    fn session_dir(&self, session: &str) -> PathBuf {
        self.root.join(escape_session_id(session))
    }

    async fn read_manifest(&self, dir: &Path, id: &str) -> crate::Result<Manifest> {
        let path = dir.join(id).join("manifest.json");
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| storage_error(format!("read {}", path.display()), e))?;
        serde_json::from_str(&text).map_err(|e| crate::error::SyscityError::Storage {
            context: format!("parse {}", path.display()),
            details: e.to_string(),
        })
    }

    async fn write_manifest(&self, dir: &Path, manifest: &Manifest) -> crate::Result<()> {
        let path = dir.join(&manifest.id).join("manifest.json");
        let text = serde_json::to_string_pretty(manifest).map_err(|e| {
            crate::error::SyscityError::Storage {
                context: "serialize checkpoint".to_string(),
                details: e.to_string(),
            }
        })?;
        tokio::fs::write(&path, text)
            .await
            .map_err(|e| storage_error(format!("write {}", path.display()), e))
    }

    async fn set_open(&self, dir: &Path, id: &str) -> crate::Result<()> {
        let path = dir.join("open.json");
        let text = serde_json::to_string(&Open { id: id.to_string() }).map_err(|e| {
            crate::error::SyscityError::Storage {
                context: "serialize the open checkpoint".to_string(),
                details: e.to_string(),
            }
        })?;
        tokio::fs::write(&path, text)
            .await
            .map_err(|e| storage_error(format!("write {}", path.display()), e))
    }
}

fn storage_error(context: String, e: std::io::Error) -> crate::error::SyscityError {
    crate::error::SyscityError::Storage {
        context,
        details: e.to_string(),
    }
}

/// A session id as one path segment.
///
/// Session ids are not file names — `ws:alice` is an ordinary one, and `:` is
/// not — so anything outside a conservative set is escaped rather than dropped.
/// Escaping is reversible, which is what keeps two sessions from sharing a
/// directory; a long id is capped and its tail hashed so a pathological one
/// cannot overflow a file name.
fn escape_session_id(session: &str) -> String {
    const MAX: usize = 100;
    let mut escaped = String::with_capacity(session.len());
    for byte in session.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                escaped.push(byte as char)
            }
            other => escaped.push_str(&format!("_{other:02X}")),
        }
    }
    if escaped.len() <= MAX {
        return escaped;
    }
    // FNV-1a over the *original*: two long ids that agree on their first 100
    // characters must not land in one directory.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in session.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{}-{hash:016x}", &escaped[..MAX])
}

/// A short random-ish suffix, so two checkpoints in the same second differ.
fn short_id() -> String {
    format!("{:06x}", rand::random::<u32>() & 0x00ff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> (CheckpointStore, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("syscity_checkpoints_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        (CheckpointStore::new(root.clone()), root)
    }

    #[tokio::test]
    async fn nothing_is_recorded_until_a_checkpoint_is_open() {
        let (store, root) = store("closed");
        let file = root.join("work.txt");
        tokio::fs::create_dir_all(&root).await.expect("dir");
        tokio::fs::write(&file, "original").await.expect("write");

        assert!(!store.capture("s1", &file).await.expect("capture"));
        assert!(store.list("s1").await.is_empty());
        assert!(store.open("s1").await.is_none());
    }

    #[tokio::test]
    async fn a_rollback_puts_the_files_back() {
        let (store, root) = store("restore");
        tokio::fs::create_dir_all(&root).await.expect("dir");
        let kept = root.join("kept.txt");
        let deleted = root.join("born.txt");
        tokio::fs::write(&kept, "before").await.expect("write");

        let checkpoint = store
            .begin("s1", Some("experiment".into()))
            .await
            .expect("begin");
        assert_eq!(store.open("s1").await.as_deref(), Some(checkpoint.id.as_str()));

        // A write into an existing file, and one that creates a file.
        assert!(store.capture("s1", &kept).await.expect("capture"));
        assert!(store.capture("s1", &deleted).await.expect("capture"));
        tokio::fs::write(&kept, "after").await.expect("write");
        tokio::fs::write(&deleted, "new").await.expect("write");

        let report = store
            .rollback("s1", &checkpoint.id)
            .await
            .expect("rollback");
        assert_eq!(report.restored, 1);
        assert_eq!(report.removed, 1);
        assert_eq!(report.dropped, 1);

        assert_eq!(tokio::fs::read_to_string(&kept).await.expect("read"), "before");
        assert!(
            !tokio::fs::try_exists(&deleted).await.unwrap_or(true),
            "a file the writes created is removed, not restored empty"
        );
        assert!(store.list("s1").await.is_empty(), "the checkpoint is spent");
        assert!(store.open("s1").await.is_none(), "and recording stops with it");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The pre-image is what the file looked like when the checkpoint opened —
    /// not what the previous write in the same checkpoint left behind.
    #[tokio::test]
    async fn the_first_write_into_a_file_keeps_the_first_copy() {
        let (store, root) = store("first_wins");
        tokio::fs::create_dir_all(&root).await.expect("dir");
        let file = root.join("file.txt");
        tokio::fs::write(&file, "v1").await.expect("write");

        let checkpoint = store.begin("s1", None).await.expect("begin");
        store.capture("s1", &file).await.expect("capture");
        tokio::fs::write(&file, "v2").await.expect("write");
        assert!(
            !store.capture("s1", &file).await.expect("capture again"),
            "the second write is already covered by the first copy"
        );
        tokio::fs::write(&file, "v3").await.expect("write");

        store
            .rollback("s1", &checkpoint.id)
            .await
            .expect("rollback");
        assert_eq!(tokio::fs::read_to_string(&file).await.expect("read"), "v1");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Rolling back to an older checkpoint drops the newer ones: they describe a
    /// state that no longer applies.
    #[tokio::test]
    async fn rolling_back_drops_the_newer_checkpoints() {
        let (store, root) = store("drops_newer");
        tokio::fs::create_dir_all(&root).await.expect("dir");
        let file = root.join("file.txt");
        tokio::fs::write(&file, "v1").await.expect("write");

        let first = store
            .begin("s1", Some("first".into()))
            .await
            .expect("begin");
        store.capture("s1", &file).await.expect("capture");
        tokio::fs::write(&file, "v2").await.expect("write");
        // Two checkpoints in the same second must still be distinct.
        let second = store
            .begin("s1", Some("second".into()))
            .await
            .expect("begin");
        assert_ne!(first.id, second.id);
        store.capture("s1", &file).await.expect("capture");

        let report = store.rollback("s1", &first.id).await.expect("rollback");
        assert_eq!(report.restored, 1);
        assert_eq!(report.dropped, 2);
        assert_eq!(tokio::fs::read_to_string(&file).await.expect("read"), "v1");
        assert!(store.list("s1").await.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn checkpoints_list_newest_first_and_sessions_do_not_share() {
        let (store, root) = store("listing");
        store
            .begin("s1", Some("earlier".into()))
            .await
            .expect("begin");
        std::thread::sleep(std::time::Duration::from_millis(5));
        store
            .begin("s1", Some("later".into()))
            .await
            .expect("begin");
        store.begin("s2", None).await.expect("begin");

        let s1 = store.list("s1").await;
        assert_eq!(s1.len(), 2);
        assert_eq!(s1[0].label.as_deref(), Some("later"), "newest first");
        assert_eq!(store.list("s2").await.len(), 1, "s2 has its own");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn escaping_a_session_id_is_reversible_and_bounded() {
        assert_eq!(escape_session_id("ws:alice"), "ws_3Aalice");
        assert_eq!(escape_session_id("plain-id_1.2"), "plain-id_1.2");

        let long = "x".repeat(500);
        let escaped = escape_session_id(&long);
        assert!(escaped.len() <= 120, "{} characters", escaped.len());
        // Two ids that agree on their first 100 characters must differ.
        let other = format!("{}y", "x".repeat(499));
        assert_ne!(escape_session_id(&long), escape_session_id(&other));
    }
}
