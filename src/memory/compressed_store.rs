//! Compressed JSONL Store — cold archival memory backend
//!
//! Stores memories as gzip-compressed JSONL files on disk.
//! Optimised for append-only writes and infrequent point reads.
//! Each file is a daily shard: `archival/YYYY-MM-DD.jsonl.gz`.
//!
//! # Design
//! - **Append-only writes**: each `store()` appends a single self-contained
//!   gzip member to the current day's shard. No read-modify-write. Concatenated
//!   gzip members are a valid gzip file per RFC 1952 and read transparently via
//!   `MultiGzDecoder`.
//! - **Side index** (`archival/_index.jsonl`): maps `memory_id → (shard,
//!   byte_offset, byte_len)`. Allows `get()` to seek + decompress a single
//!   member instead of loading all shards.
//! - **`update()` / `delete()` / `cleanup_expired`** still trigger a full-shard
//!   rewrite (each entry becomes its own gzip member) and rebuild the index.
//!   These operations are rare on cold archival data.
//! - **`search()` / `stats()`** still walk all shards. Archival full-text
//!   search is intentionally slow; hybrid search should reach warmer tiers
//!   first.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, RwLock};
use tokio::task::spawn_blocking;
use tracing::{debug, info, warn};

use super::{Memory, MemoryEntryType, MemoryId, MemoryQuery, MemoryStats, MemoryStore};

/// Directory name for archival shards.
pub const ARCHIVAL_DIR_NAME: &str = "archival";

/// Filename for the side index (uncompressed JSONL, one entry per line).
const INDEX_FILE_NAME: &str = "_index.jsonl";

/// Side-index entry mapping a memory id to its location in a shard,
/// plus filterable metadata to avoid loading all shards for searches.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexEntry {
    /// Memory id (string form of `MemoryId`).
    id: String,
    /// Shard file name (e.g. `2026-07-01.jsonl.gz`).
    shard: String,
    /// Byte offset within the shard where this entry's gzip member begins.
    offset: u64,
    /// Byte length of this entry's gzip member.
    len: u64,
    /// User id for filtering (avoids loading shard if mismatch).
    user_id: String,
    /// Memory type for filtering (avoids loading shard if mismatch).
    memory_type: MemoryEntryType,
    /// Creation time for reference.
    created_at_secs: i64,
    /// Last access time for reference.
    last_accessed_secs: i64,
    /// Access count for reference.
    access_count: u64,
    /// Expiration time for filtering (None = never expires).
    expires_at_secs: Option<i64>,
}

/// Gzip-compressed JSONL memory store for cold archival storage.
#[derive(Debug)]
pub struct CompressedJsonlStore {
    /// Base directory (e.g. `{workspace}/memory/archival/`)
    dir: PathBuf,
    /// Maximum memories per shard before rotating to next day.
    max_per_shard: usize,
    /// Mutex for synchronising write operations (store, update, delete,
    /// cleanup). Prevents concurrent read-modify-write cycles from silently
    /// losing data.
    write_lock: Arc<Mutex<()>>,
    /// In-memory cache of the side index. `None` = not yet loaded.
    index: Arc<RwLock<Option<HashMap<String, IndexEntry>>>>,
}

impl Clone for CompressedJsonlStore {
    fn clone(&self) -> Self {
        Self {
            dir: self.dir.clone(),
            max_per_shard: self.max_per_shard,
            write_lock: Arc::clone(&self.write_lock),
            index: Arc::clone(&self.index),
        }
    }
}

impl CompressedJsonlStore {
    /// Create a new compressed store at the given base path.
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        Self {
            dir: base_dir.as_ref().join(ARCHIVAL_DIR_NAME),
            max_per_shard: 10_000,
            write_lock: Arc::new(Mutex::new(())),
            index: Arc::new(RwLock::new(None)),
        }
    }

    /// Builder: set max memories per shard.
    pub fn with_max_per_shard(mut self, n: usize) -> Self {
        self.max_per_shard = n;
        self
    }

    /// Ensure the archival directory exists.
    async fn ensure_dir(&self) -> crate::Result<()> {
        fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to create archival dir: {:?}", self.dir),
                details: e.to_string(),
            })
    }

    /// Shard path for today's date.
    fn today_shard(&self) -> PathBuf {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        self.dir.join(format!("{}.jsonl.gz", today))
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join(INDEX_FILE_NAME)
    }

    /// List all shard files, sorted oldest first.
    async fn list_shards(&self) -> Vec<PathBuf> {
        let mut shards = Vec::new();
        let Ok(mut read_dir) = fs::read_dir(&self.dir).await else {
            return shards;
        };
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("gz") {
                shards.push(path);
            }
        }
        shards.sort();
        shards
    }

    /// Load (or return cached) side index.
    ///
    /// If the index file is missing but shards exist (legacy data written
    /// before the index existed), it is rebuilt by scanning shard contents.
    async fn load_index(&self) -> crate::Result<HashMap<String, IndexEntry>> {
        {
            let guard = self.index.read().await;
            if let Some(idx) = guard.as_ref() {
                return Ok(idx.clone());
            }
        }

        let idx_path = self.index_path();
        let map = if idx_path.exists() {
            let text = fs::read_to_string(&idx_path).await.map_err(|e| {
                crate::error::SyscityError::Storage {
                    context: format!("Failed to read archival index: {:?}", idx_path),
                    details: e.to_string(),
                }
            })?;
            let mut map = HashMap::new();
            for (line_no, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<IndexEntry>(line) {
                    Ok(entry) => {
                        // Later entries overwrite earlier ones (last write wins).
                        map.insert(entry.id.clone(), entry);
                    }
                    Err(e) => {
                        warn!(
                            "Skipping malformed index line {} in {:?}: {}",
                            line_no + 1,
                            idx_path,
                            e
                        );
                    }
                }
            }
            map
        } else {
            // Legacy: index missing. Rebuild by scanning shards. This is a one-off
            // O(archive size) cost; subsequent calls will use the cache.
            self.rebuild_index_from_shards().await?
        };

        let mut guard = self.index.write().await;
        *guard = Some(map.clone());
        Ok(map)
    }

    /// Invalidate cached index (call after external file changes).
    async fn invalidate_index_cache(&self) {
        *self.index.write().await = None;
    }

    /// Append one entry to the on-disk index file and update the in-memory
    /// cache.
    async fn append_index_entry(&self, entry: IndexEntry) -> crate::Result<()> {
        let idx_path = self.index_path();
        let line =
            serde_json::to_string(&entry).map_err(crate::error::SyscityError::Serialization)?;

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&idx_path)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to open archival index for append: {:?}", idx_path),
                details: e.to_string(),
            })?;
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        file.write_all(&bytes)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to write archival index: {:?}", idx_path),
                details: e.to_string(),
            })?;
        file.sync_all()
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to fsync archival index: {:?}", idx_path),
                details: e.to_string(),
            })?;

        // Update cache.
        let mut guard = self.index.write().await;
        if let Some(map) = guard.as_mut() {
            map.insert(entry.id.clone(), entry);
        } else {
            let mut map = HashMap::new();
            map.insert(entry.id.clone(), entry);
            *guard = Some(map);
        }
        Ok(())
    }

    /// Convert SystemTime to seconds since Unix epoch for index storage.
    fn system_time_to_secs(t: std::time::SystemTime) -> i64 {
        t.duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Check if a memory (represented by index entry) is expired.
    fn is_expired(entry: &IndexEntry) -> bool {
        if let Some(exp_secs) = entry.expires_at_secs {
            let now_secs = Self::system_time_to_secs(std::time::SystemTime::now());
            now_secs > exp_secs
        } else {
            false
        }
    }

    /// Rebuild the index by scanning every shard, decompressing each member,
    /// and recording (id, shard, offset, len) for each entry, plus filterable
    /// metadata.
    ///
    /// Used when the index file is missing (legacy data) or after a rewrite.
    /// The rebuilt index is written atomically to `_index.jsonl`.
    async fn rebuild_index_from_shards(&self) -> crate::Result<HashMap<String, IndexEntry>> {
        let shards = self.list_shards().await;
        let mut map: HashMap<String, IndexEntry> = HashMap::new();

        if shards.is_empty() {
            // No data yet — nothing to persist and no directory guaranteed to exist.
            return Ok(map);
        }

        for shard in shards {
            let shard_name = shard
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let data = fs::read(&shard)
                .await
                .map_err(|e| crate::error::SyscityError::Storage {
                    context: format!("Failed to read archival shard: {:?}", shard),
                    details: e.to_string(),
                })?;

            let members = Self::split_gzip_members(data).await?;
            let mut offset: u64 = 0;
            for member in members {
                let len = member.len() as u64;
                let lines = Self::decompress_bytes(member).await?;
                for line in lines {
                    if let Ok(mem) = serde_json::from_str::<Memory>(&line) {
                        map.insert(
                            mem.id.0.clone(),
                            IndexEntry {
                                id: mem.id.0.clone(),
                                shard: shard_name.clone(),
                                offset,
                                len,
                                user_id: mem.user_id.clone(),
                                memory_type: mem.memory_type.clone(),
                                created_at_secs: Self::system_time_to_secs(mem.created_at),
                                last_accessed_secs: Self::system_time_to_secs(mem.last_accessed),
                                access_count: mem.access_count,
                                expires_at_secs: mem.expires_at.map(Self::system_time_to_secs),
                            },
                        );
                    }
                }
                offset += len;
            }
        }

        // Persist rebuilt index atomically. Ensure the directory exists first so
        // this cannot fail on a fresh (never-written-to) archival dir.
        self.ensure_dir().await?;
        self.write_index_atomic(&map).await?;
        Ok(map)
    }

    /// Write the entire index atomically (temp file + rename).
    async fn write_index_atomic(&self, map: &HashMap<String, IndexEntry>) -> crate::Result<()> {
        let idx_path = self.index_path();
        let tmp_path = self
            .dir
            .join(format!(".{}.tmp.{}", INDEX_FILE_NAME, std::process::id()));

        let mut buf = String::new();
        for entry in map.values() {
            let line =
                serde_json::to_string(entry).map_err(crate::error::SyscityError::Serialization)?;
            buf.push_str(&line);
            buf.push('\n');
        }
        fs::write(&tmp_path, buf)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to write temp index: {:?}", tmp_path),
                details: e.to_string(),
            })?;
        fs::rename(&tmp_path, &idx_path).await.map_err(|e| {
            crate::error::SyscityError::Storage {
                context: format!("Failed to rename temp index to {:?}", idx_path),
                details: e.to_string(),
            }
        })?;
        Ok(())
    }

    /// Split a gzip file (possibly containing multiple concatenated members)
    /// into individual member byte slices.
    ///
    /// This is needed to compute per-member byte offsets when rebuilding the
    /// index. Each member's extent is found exactly: parse the RFC 1952
    /// header, raw-inflate the deflate stream with `mem::Decompress` until
    /// `StreamEnd` (`total_in()` is then the stream's exact length), and skip
    /// the fixed 8-byte trailer. Scanning for the gzip magic (0x1F 0x8B)
    /// instead would false-positive on that byte sequence inside deflate
    /// payloads — the "member" from such a start is truncated garbage and
    /// fails to decompress with "unexpected end of file", which made index
    /// rebuilds (and therefore the first `store()` on a legacy dir) flaky
    /// depending on the compressed bytes' content.
    async fn split_gzip_members(data: Vec<u8>) -> crate::Result<Vec<Vec<u8>>> {
        spawn_blocking(move || split_gzip_members_sync(&data))
            .await
            .map_err(|e| {
                crate::error::SyscityError::Validation(format!("split_gzip task panicked: {}", e))
            })?
    }

    /// Compress a single JSON line into its own gzip member.
    async fn compress_line(line: String) -> crate::Result<Vec<u8>> {
        Self::compress_lines(vec![line]).await
    }

    /// Append a single memory to the current shard as its own gzip member,
    /// updating the side index.
    async fn append_one(&self, memory: &Memory) -> crate::Result<()> {
        self.ensure_dir().await?;

        let shard = self.today_shard();
        let shard_name = shard
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| crate::error::SyscityError::Storage {
                context: "Invalid shard path".to_string(),
                details: format!("{:?}", shard),
            })?
            .to_string();

        let line =
            serde_json::to_string(memory).map_err(crate::error::SyscityError::Serialization)?;
        let compressed = Self::compress_line(line).await?;
        let len = compressed.len() as u64;

        // Open in append mode; O_APPEND guarantees writes land at end of file
        // atomically (per POSIX), but we still hold `write_lock` for a
        // consistent offset -> content mapping vs. the index update.
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&shard)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to open shard for append: {:?}", shard),
                details: e.to_string(),
            })?;
        let offset = file
            .metadata()
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to stat shard: {:?}", shard),
                details: e.to_string(),
            })?
            .len();
        file.write_all(&compressed)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to append to shard: {:?}", shard),
                details: e.to_string(),
            })?;
        file.sync_all()
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to fsync shard: {:?}", shard),
                details: e.to_string(),
            })?;

        // Warn if shard is growing beyond soft limit; do not enforce.
        let approx_entries = self
            .load_index()
            .await?
            .values()
            .filter(|e| e.shard == shard_name)
            .count()
            + 1;
        if approx_entries > self.max_per_shard {
            warn!(
                "Archival shard {:?} exceeded max_per_shard ({}); continuing anyway",
                shard, self.max_per_shard
            );
        }

        self.append_index_entry(IndexEntry {
            id: memory.id.0.clone(),
            shard: shard_name,
            offset,
            len,
            user_id: memory.user_id.clone(),
            memory_type: memory.memory_type.clone(),
            created_at_secs: Self::system_time_to_secs(memory.created_at),
            last_accessed_secs: Self::system_time_to_secs(memory.last_accessed),
            access_count: memory.access_count,
            expires_at_secs: memory.expires_at.map(Self::system_time_to_secs),
        })
        .await?;

        Ok(())
    }

    /// Decompress a byte slice that may contain one or more gzip members.
    async fn decompress_bytes(data: Vec<u8>) -> crate::Result<Vec<String>> {
        spawn_blocking(move || {
            use flate2::read::MultiGzDecoder;
            let decoder = MultiGzDecoder::new(data.as_slice());
            let reader = std::io::BufReader::new(decoder);
            let mut lines = Vec::new();
            for line in reader.lines() {
                let line = line.map_err(|e| crate::error::SyscityError::Storage {
                    context: "Failed to read line from archival shard".to_string(),
                    details: e.to_string(),
                })?;
                if !line.trim().is_empty() {
                    lines.push(line);
                }
            }
            Ok(lines)
        })
        .await
        .map_err(|e| {
            crate::error::SyscityError::Validation(format!("decompress task panicked: {}", e))
        })?
    }

    /// Compress lines as a single gzip member.
    async fn compress_lines(lines: Vec<String>) -> crate::Result<Vec<u8>> {
        spawn_blocking(move || {
            use flate2::write::GzEncoder;
            use flate2::Compression;
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            for line in lines {
                writeln!(encoder, "{}", line).map_err(|e| crate::error::SyscityError::Storage {
                    context: "Failed to write line to archival shard".to_string(),
                    details: e.to_string(),
                })?;
            }
            encoder
                .finish()
                .map_err(|e| crate::error::SyscityError::Storage {
                    context: "Failed to finish gzip compression".to_string(),
                    details: e.to_string(),
                })
        })
        .await
        .map_err(|e| {
            crate::error::SyscityError::Validation(format!("compress task panicked: {}", e))
        })?
    }

    /// Read `len` bytes at `offset` from a shard.
    async fn read_range(path: &Path, offset: u64, len: u64) -> crate::Result<Vec<u8>> {
        let mut file =
            fs::File::open(path)
                .await
                .map_err(|e| crate::error::SyscityError::Storage {
                    context: format!("Failed to open shard for read: {:?}", path),
                    details: e.to_string(),
                })?;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to seek shard: {:?}", path),
                details: e.to_string(),
            })?;
        let mut buf = vec![0u8; len as usize];
        file.read_exact(&mut buf)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to read shard bytes: {:?}", path),
                details: e.to_string(),
            })?;
        Ok(buf)
    }

    /// Load all memories from all shards (used by search/stats/rewrite).
    async fn load_all(&self) -> crate::Result<Vec<Memory>> {
        let shards = self.list_shards().await;
        let mut memories = Vec::new();
        for shard in shards {
            let data = fs::read(&shard)
                .await
                .map_err(|e| crate::error::SyscityError::Storage {
                    context: format!("Failed to read archival shard: {:?}", shard),
                    details: e.to_string(),
                })?;
            let lines = Self::decompress_bytes(data).await?;
            for line in lines {
                match serde_json::from_str::<Memory>(&line) {
                    Ok(mem) => memories.push(mem),
                    Err(e) => {
                        warn!("Skipping malformed archival entry: {}", e);
                    }
                }
            }
        }
        Ok(memories)
    }

    /// Clean up orphaned `.tmp.*` files left by crashed processes.
    ///
    /// Should be called once at application startup.
    pub async fn cleanup_orphan_temps(&self) {
        let Ok(mut read_dir) = fs::read_dir(&self.dir).await else {
            return;
        };
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') && name_str.contains(".tmp.") {
                if let Err(e) = fs::remove_file(entry.path()).await {
                    warn!("Failed to remove orphan temp file {:?}: {}", entry.path(), e);
                } else {
                    debug!("Cleaned up orphan temp file {:?}", entry.path());
                }
            }
        }
    }

    /// Rewrite all shards with the given memories (used by delete/update).
    ///
    /// Each entry is written as its own gzip member so per-id offsets stay
    /// meaningful. Uses a two-phase commit: all temp shards + the temp index
    /// are written first, then renamed atomically.
    async fn rewrite_all(&self, memories: Vec<Memory>) -> crate::Result<()> {
        // Group by shard date based on created_at.
        let mut by_date: HashMap<String, Vec<Memory>> = HashMap::new();
        for mem in memories {
            let date = chrono::DateTime::<chrono::Utc>::from(mem.created_at)
                .format("%Y-%m-%d")
                .to_string();
            by_date.entry(date).or_default().push(mem);
        }

        let old_shards = self.list_shards().await;
        self.cleanup_orphan_temps().await;

        // Phase 1: build compressed data for each shard, tracking per-entry offsets.
        let mut pending: Vec<(PathBuf, PathBuf, Vec<u8>)> = Vec::new(); // (tmp, final, bytes)
        let mut new_index: HashMap<String, IndexEntry> = HashMap::new();

        for (date, mems) in &by_date {
            let shard = self.dir.join(format!("{}.jsonl.gz", date));
            let shard_name = format!("{}.jsonl.gz", date);
            let mut shard_bytes: Vec<u8> = Vec::new();

            for m in mems {
                let line =
                    serde_json::to_string(m).map_err(crate::error::SyscityError::Serialization)?;
                let member = Self::compress_line(line).await?;
                let offset = shard_bytes.len() as u64;
                let len = member.len() as u64;
                shard_bytes.extend_from_slice(&member);
                new_index.insert(
                    m.id.0.clone(),
                    IndexEntry {
                        id: m.id.0.clone(),
                        shard: shard_name.clone(),
                        offset,
                        len,
                        user_id: m.user_id.clone(),
                        memory_type: m.memory_type.clone(),
                        created_at_secs: Self::system_time_to_secs(m.created_at),
                        last_accessed_secs: Self::system_time_to_secs(m.last_accessed),
                        access_count: m.access_count,
                        expires_at_secs: m.expires_at.map(Self::system_time_to_secs),
                    },
                );
            }

            let tmp_path = self.dir.join(format!(
                ".{}.tmp.{}",
                shard.file_name().unwrap_or_default().to_string_lossy(),
                std::process::id()
            ));
            fs::write(&tmp_path, &shard_bytes).await.map_err(|e| {
                crate::error::SyscityError::Storage {
                    context: format!("Failed to write temp shard: {:?}", tmp_path),
                    details: e.to_string(),
                }
            })?;
            pending.push((tmp_path, shard, shard_bytes));
        }

        // Also stage the new index file.
        let idx_tmp = self
            .dir
            .join(format!(".{}.tmp.{}", INDEX_FILE_NAME, std::process::id()));
        let mut idx_buf = String::new();
        for entry in new_index.values() {
            let line =
                serde_json::to_string(entry).map_err(crate::error::SyscityError::Serialization)?;
            idx_buf.push_str(&line);
            idx_buf.push('\n');
        }
        fs::write(&idx_tmp, idx_buf)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to write temp index: {:?}", idx_tmp),
                details: e.to_string(),
            })?;

        // Phase 2: rename shard temp files.
        let mut renamed: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (tmp_path, shard, _) in &pending {
            match fs::rename(tmp_path, shard).await {
                Ok(_) => renamed.push((shard.clone(), tmp_path.clone())),
                Err(e) => {
                    for (done_shard, done_tmp) in &renamed {
                        if let Err(rb) = fs::rename(done_shard, done_tmp).await {
                            warn!(
                                "Rollback failed for {:?} -> {:?}: {}. Data may be inconsistent.",
                                done_shard, done_tmp, rb
                            );
                        }
                    }
                    let _ = fs::remove_file(&idx_tmp).await;
                    return Err(crate::error::SyscityError::Storage {
                        context: format!(
                            "Failed to rename temp shard to {:?} (rolled back {} previous)",
                            shard,
                            renamed.len()
                        ),
                        details: e.to_string(),
                    });
                }
            }
        }

        // Rename index last so a partial rewrite still leaves a consistent
        // index-vs-shards state.
        if let Err(e) = fs::rename(&idx_tmp, self.index_path()).await {
            warn!(
                "Failed to install rebuilt index at {:?}: {}. Cache invalidated; next \
                 load_index() will rebuild from shards.",
                self.index_path(),
                e
            );
            let _ = fs::remove_file(&idx_tmp).await;
        }

        // Remove old shards that were not rewritten.
        for old in &old_shards {
            if !renamed.iter().any(|(shard, _)| shard == old) {
                let mut last_err = None;
                for attempt in 1..=3 {
                    match fs::remove_file(old).await {
                        Ok(_) => {
                            last_err = None;
                            break;
                        }
                        Err(e) => {
                            last_err = Some(e);
                            if attempt < 3 {
                                tokio::time::sleep(std::time::Duration::from_millis(50 * attempt))
                                    .await;
                            }
                        }
                    }
                }
                if let Some(e) = last_err {
                    warn!(
                        "Failed to remove stale archival shard {:?} after 3 attempts: {}",
                        old, e
                    );
                }
            }
        }

        // Refresh cache.
        *self.index.write().await = Some(new_index);
        Ok(())
    }
}

#[async_trait]
impl MemoryStore for CompressedJsonlStore {
    async fn store(&self, memory: Memory) -> crate::Result<MemoryId> {
        let id = memory.id.clone();
        let _guard = self.write_lock.lock().await;
        self.append_one(&memory).await?;
        drop(_guard);
        debug!("Archived memory: {}", id);
        Ok(id)
    }

    async fn get(&self, id: &MemoryId) -> crate::Result<Option<Memory>> {
        let index = self.load_index().await?;
        let Some(entry) = index.get(&id.0).cloned() else {
            return Ok(None);
        };
        let shard_path = self.dir.join(&entry.shard);
        let bytes = match Self::read_range(&shard_path, entry.offset, entry.len).await {
            Ok(b) => b,
            Err(e) => {
                // Index points at a stale shard; drop cache and try scanning as fallback.
                warn!(
                    "Archival index miss for {}: {} — invalidating cache and scanning shards",
                    id, e
                );
                self.invalidate_index_cache().await;
                return Ok(self.load_all().await?.into_iter().find(|m| m.id == *id));
            }
        };
        let lines = Self::decompress_bytes(bytes).await?;
        if let Some(line) = lines.first() {
            return Ok(Some(
                serde_json::from_str(line).map_err(crate::error::SyscityError::Serialization)?,
            ));
        }
        Ok(None)
    }

    async fn update(&self, memory: Memory) -> crate::Result<()> {
        let _guard = self.write_lock.lock().await;
        let mut all = self.load_all().await?;
        let mut found = false;
        for m in &mut all {
            if m.id == memory.id {
                *m = memory.clone();
                found = true;
                break;
            }
        }
        if !found {
            return Err(crate::error::SyscityError::NotFound {
                resource: format!("Memory {}", memory.id),
            });
        }
        self.rewrite_all(all).await
    }

    async fn update_importance_score(
        &self,
        id: &MemoryId,
        new_score: f32,
    ) -> crate::Result<Option<Memory>> {
        let _guard = self.write_lock.lock().await;
        let mut all = self.load_all().await?;
        let Some(idx) = all.iter().position(|m| m.id == *id) else {
            return Ok(None);
        };
        if (all[idx].importance_score - new_score).abs() >= 0.001 {
            all[idx].importance_score = new_score;
            let updated = all[idx].clone();
            self.rewrite_all(all).await?;
            return Ok(Some(updated));
        }
        Ok(Some(all[idx].clone()))
    }

    async fn delete(&self, id: &MemoryId) -> crate::Result<bool> {
        let _guard = self.write_lock.lock().await;
        let mut all = self.load_all().await?;
        let before = all.len();
        all.retain(|m| m.id != *id);
        let removed = before - all.len();
        if removed > 0 {
            self.rewrite_all(all).await?;
        }
        Ok(removed > 0)
    }

    /// Search for memories using the index to avoid loading all shards.
    async fn search(&self, query: MemoryQuery) -> crate::Result<Vec<Memory>> {
        // First filter index entries in memory to avoid loading irrelevant shards.
        let index = self.load_index().await?;
        let filtered_index_entries: Vec<&IndexEntry> = index
            .values()
            .filter(|entry| {
                // Apply filters that use index metadata first.
                if let Some(ref user_id) = query.user_id {
                    if entry.user_id != *user_id {
                        return false;
                    }
                }
                if let Some(ref mem_type) = query.memory_type {
                    if entry.memory_type != *mem_type {
                        return false;
                    }
                }
                if !query.include_expired && Self::is_expired(entry) {
                    return false;
                }
                true
            })
            .collect();

        // Group entries by shard to load each shard at most once.
        let mut by_shard: HashMap<&str, Vec<&IndexEntry>> = HashMap::new();
        for entry in filtered_index_entries {
            by_shard.entry(&entry.shard).or_default().push(entry);
        }

        // Load only relevant entries from relevant shards.
        let mut candidates = Vec::new();
        for (shard_name, entries) in by_shard {
            let shard_path = self.dir.join(shard_name);
            if !shard_path.exists() {
                continue;
            }
            for entry in entries {
                match Self::read_range(&shard_path, entry.offset, entry.len).await {
                    Ok(bytes) => {
                        if let Ok(lines) = Self::decompress_bytes(bytes).await {
                            if let Some(line) = lines.first() {
                                if let Ok(mem) = serde_json::from_str::<Memory>(line) {
                                    candidates.push(mem);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            "Failed to read archival entry {} from {}: {}",
                            entry.id, shard_name, e
                        );
                    }
                }
            }
        }

        // Apply remaining filters (content_query and conversation_id, which aren't in
        // index), plus double-check include_expired in case system time
        // changed.
        let mut results: Vec<Memory> = candidates
            .into_iter()
            .filter(|m| {
                if let Some(ref conv_id) = query.conversation_id {
                    if m.conversation_id.as_ref() != Some(conv_id) {
                        return false;
                    }
                }
                if let Some(ref content) = query.content_query {
                    if !m.content.to_lowercase().contains(&content.to_lowercase()) {
                        return false;
                    }
                }
                if !query.include_expired && m.is_expired() {
                    return false;
                }
                true
            })
            .collect();

        // Sort by importance descending, apply offset/limit.
        results.sort_by(|a, b| {
            b.importance_score
                .partial_cmp(&a.importance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let offset = query.offset.min(results.len());
        let limit = query.limit.min(results.len() - offset);
        Ok(results.into_iter().skip(offset).take(limit).collect())
    }

    async fn cleanup_expired(&self) -> crate::Result<usize> {
        let _guard = self.write_lock.lock().await;
        let mut all = self.load_all().await?;
        let before = all.len();
        all.retain(|m| !m.is_expired());
        let removed = before - all.len();
        if removed > 0 {
            self.rewrite_all(all).await?;
            info!("Cleaned up {} expired archival memories", removed);
        }
        Ok(removed)
    }

    async fn stats(&self) -> crate::Result<MemoryStats> {
        let all = self.load_all().await?;
        let total = all.len();
        let mut count_by_type = HashMap::new();
        let mut expired = 0;
        for m in &all {
            *count_by_type.entry(m.memory_type.clone()).or_insert(0) += 1;
            if m.is_expired() {
                expired += 1;
            }
        }
        Ok(MemoryStats {
            total_count: total,
            count_by_type,
            expired_count: expired,
        })
    }

    async fn close(&self) -> crate::Result<()> {
        info!("CompressedJsonlStore closed");
        Ok(())
    }
}

/// Length of a gzip member's trailer: CRC32 + ISIZE (RFC 1952 §2.3.1).
const GZIP_TRAILER_LEN: usize = 8;

/// Return the length of the RFC 1952 member header starting at `start`, or
/// `None` when the bytes do not begin a plausible gzip member (magic, deflate
/// method, and the optional fields FEXTRA/FNAME/FCOMMENT/FHCRC accounted for).
fn gzip_header_len(data: &[u8], start: usize) -> Option<usize> {
    if start + 10 > data.len() || data[start] != 0x1F || data[start + 1] != 0x8B {
        return None;
    }
    // CM must be 8 ("deflate"); we only ever write members that satisfy this.
    if data[start + 2] != 8 {
        return None;
    }
    let flg = data[start + 3];
    let mut p = start + 10;
    if flg & 0x04 != 0 {
        // FEXTRA: 2-byte LE length followed by that many bytes.
        if p + 2 > data.len() {
            return None;
        }
        let len = u16::from_le_bytes([data[p], data[p + 1]]) as usize;
        p += 2 + len;
    }
    if flg & 0x08 != 0 {
        // FNAME: zero-terminated string.
        if p >= data.len() {
            return None;
        }
        p += data[p..].iter().position(|&b| b == 0)? + 1;
    }
    if flg & 0x10 != 0 {
        // FCOMMENT: zero-terminated string.
        if p >= data.len() {
            return None;
        }
        p += data[p..].iter().position(|&b| b == 0)? + 1;
    }
    if flg & 0x02 != 0 {
        p += 2; // FHCRC
    }
    if p > data.len() {
        return None;
    }
    Some(p - start)
}

/// Split concatenated gzip members into exact member byte slices (see
/// `CompressedJsonlStore::split_gzip_members`).
fn split_gzip_members_sync(data: &[u8]) -> crate::Result<Vec<Vec<u8>>> {
    use flate2::{Decompress, FlushDecompress, Status};

    let mut members = Vec::new();
    let mut pos = 0usize;

    while pos + 2 <= data.len() {
        let Some(hdr_len) = gzip_header_len(data, pos) else {
            // Sequentially decoded members always resume at a real header, so
            // a miss here means corrupt or foreign bytes; skip one and keep
            // looking (same tolerance as the old magic scan).
            pos += 1;
            continue;
        };

        // Raw-inflate the deflate stream: `StreamEnd` marks the exact end of
        // the member's deflate body regardless of what bytes follow.
        let deflate_start = pos + hdr_len;
        let mut de = Decompress::new(false);
        let mut out: Vec<u8> = Vec::new();
        let mut consumed = 0usize;
        let mut done = false;
        loop {
            let input = &data[deflate_start + consumed..];
            if input.is_empty() {
                break; // truncated member: deflate stream never ended
            }
            if out.len() == out.capacity() {
                out.reserve(4096);
            }
            match de.decompress_vec(input, &mut out, FlushDecompress::None) {
                Ok(Status::StreamEnd) => {
                    consumed = de.total_in() as usize;
                    done = true;
                    break;
                }
                Ok(_) => {
                    let new_consumed = de.total_in() as usize;
                    if new_consumed == consumed && out.len() == out.capacity() {
                        out.reserve(out.len().max(4096));
                    }
                    consumed = new_consumed;
                }
                Err(e) => {
                    if deflate_start + consumed >= data.len() {
                        break; // truncated tail, handled below
                    }
                    return Err(crate::error::SyscityError::Storage {
                        context: format!("Corrupt gzip member at offset {} in archival shard", pos),
                        details: e.to_string(),
                    });
                }
            }
        }

        let member_end = deflate_start + consumed + GZIP_TRAILER_LEN;
        if !done || member_end > data.len() {
            // Truncated final member (e.g. a crash mid-append): ignore the
            // tail instead of failing the whole scan.
            warn!(
                "Ignoring truncated final gzip member in archival shard \
                 ({} trailing bytes)",
                data.len() - pos
            );
            break;
        }

        members.push(data[pos..member_end].to_vec());
        pos = member_end;
    }

    Ok(members)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn test_compressed_store_roundtrip() {
        let dir = tempdir().unwrap();
        let store = CompressedJsonlStore::new(dir.path());

        let mem = Memory::new("u1", "Archival fact", "fact").with_importance_score(0.9);
        let id = store.store(mem.clone()).await.unwrap();

        let fetched = store.get(&id).await.unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().content, "Archival fact");
    }

    #[tokio::test]
    async fn test_compressed_search() {
        let dir = tempdir().unwrap();
        let store = CompressedJsonlStore::new(dir.path());

        store
            .store(Memory::new("u1", "Old project notes", "note"))
            .await
            .unwrap();
        store
            .store(Memory::new("u1", "Very old diary entry", "diary"))
            .await
            .unwrap();
        store
            .store(Memory::new("u2", "Another user's note", "note"))
            .await
            .unwrap();

        let results = store
            .search(MemoryQuery::new().for_user("u1").with_content("old"))
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_compressed_delete_and_rewrite() {
        let dir = tempdir().unwrap();
        let store = CompressedJsonlStore::new(dir.path());

        let mem1 = Memory::new("u1", "Keep me", "fact");
        let mem2 = Memory::new("u1", "Delete me", "fact");
        let id1 = store.store(mem1).await.unwrap();
        let id2 = store.store(mem2).await.unwrap();

        let deleted = store.delete(&id2).await.unwrap();
        assert!(deleted);

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.total_count, 1);

        let remaining = store.get(&id1).await.unwrap();
        assert!(remaining.is_some());
    }

    #[tokio::test]
    async fn test_append_only_multiple_writes() {
        // After N appends, get() must find every id using the side index
        // (i.e. without falling back to full scan).
        let dir = tempdir().unwrap();
        let store = CompressedJsonlStore::new(dir.path());

        let mut ids = Vec::new();
        for i in 0..20 {
            let m = Memory::new("u1", format!("entry #{i}"), "fact");
            ids.push(store.store(m).await.unwrap());
        }

        for (i, id) in ids.iter().enumerate() {
            let m = store.get(id).await.unwrap();
            assert!(m.is_some(), "missing entry {i}");
            assert_eq!(m.unwrap().content, format!("entry #{i}"));
        }
    }

    #[tokio::test]
    async fn test_index_rebuild_from_legacy_shards() {
        // Simulate legacy data: shards exist but _index.jsonl is missing.
        let dir = tempdir().unwrap();
        let store = CompressedJsonlStore::new(dir.path());
        let id1 = store
            .store(Memory::new("u1", "first", "fact"))
            .await
            .unwrap();
        let id2 = store
            .store(Memory::new("u1", "second", "fact"))
            .await
            .unwrap();

        // Delete the on-disk index to simulate legacy state.
        let idx = store.index_path();
        assert!(idx.exists());
        std::fs::remove_file(&idx).unwrap();
        store.invalidate_index_cache().await;

        // Reads must still work; index gets rebuilt.
        let m1 = store.get(&id1).await.unwrap();
        assert!(m1.is_some(), "rebuild failed for id1");
        let m2 = store.get(&id2).await.unwrap();
        assert!(m2.is_some(), "rebuild failed for id2");
        assert!(idx.exists(), "rebuild should recreate the index file");
    }

    #[tokio::test]
    async fn split_gzip_members_tolerates_inner_magic_bytes() {
        // The gzip magic (1F 8B) can occur inside a member's deflate payload.
        // The old magic-scan split treated such an occurrence as the next
        // member boundary, producing a truncated "member" that failed to
        // decompress ("unexpected end of file") and broke index rebuilds.
        // Find a payload whose compressed output exhibits the pattern, then
        // assert the decode-driven split still yields exactly the members.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut inner_member = None;
        for i in 0..20000 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let line =
                format!("entry #{i} {:016x} {:016x} {:016x} {:016x}", state, state, state, state);
            let member = CompressedJsonlStore::compress_line(line).await.unwrap();
            let has_inner = (2..member.len().saturating_sub(1))
                .any(|i| member[i] == 0x1F && member[i + 1] == 0x8B);
            if has_inner {
                inner_member = Some(member);
                break;
            }
        }
        let Some(inner_member) = inner_member else {
            // No compressed payload exhibited the pattern; nothing to prove.
            return;
        };

        let head = CompressedJsonlStore::compress_line("first".into())
            .await
            .unwrap();
        let tail = CompressedJsonlStore::compress_line("last".into())
            .await
            .unwrap();
        let mut data = head;
        data.extend_from_slice(&inner_member);
        data.extend_from_slice(&tail);

        let members = CompressedJsonlStore::split_gzip_members(data.clone())
            .await
            .unwrap();
        assert_eq!(members.len(), 3, "inner magic must not split a member");
        assert_eq!(members.concat(), data, "slices must tile the input");

        let first = CompressedJsonlStore::decompress_bytes(members[0].clone())
            .await
            .unwrap();
        assert_eq!(first, vec!["first"]);
        let last = CompressedJsonlStore::decompress_bytes(members[2].clone())
            .await
            .unwrap();
        assert_eq!(last, vec!["last"]);
    }

    #[tokio::test]
    async fn split_gzip_members_is_exact_across_varied_payloads() {
        // Decode-driven splitting must tile the input exactly and decompress
        // to the original lines for arbitrary payloads.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let lines: Vec<String> = (0..40)
            .map(|i| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                format!("entry #{i} {:016x}", state)
            })
            .collect();

        let mut data = Vec::new();
        for line in &lines {
            data.extend(
                CompressedJsonlStore::compress_line(line.clone())
                    .await
                    .unwrap(),
            );
        }

        let members = CompressedJsonlStore::split_gzip_members(data.clone())
            .await
            .unwrap();
        assert_eq!(members.len(), lines.len());
        assert_eq!(members.concat(), data);
        for (member, line) in members.iter().zip(&lines) {
            let out = CompressedJsonlStore::decompress_bytes(member.clone())
                .await
                .unwrap();
            assert_eq!(out, vec![line.as_str()]);
        }
    }
}
