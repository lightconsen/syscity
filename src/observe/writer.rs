//! Persists turn observability records to disk.
//!
//! Layout: `<base>/YYYY-MM-DD/<turn_id>/` (local-time date partition), with a
//! truncated `summary.json` and an untruncated append-only `full.json` JSONL.
//! Writes are atomic (tmp file + rename). Both an async and a blocking variant
//! are provided — the blocking one backs the collector's `Drop` fallback where
//! no async context may exist.

use std::path::{Path, PathBuf};

use crate::error::{Result, SyscityError};

use super::record::{FullTraceEvent, TurnRecord};

pub struct TurnMetricsWriter {
    base: PathBuf,
}

impl TurnMetricsWriter {
    pub fn new(base: PathBuf) -> Self {
        Self { base }
    }

    /// Default writer rooted at `~/.syscity/turns`.
    pub fn default_dir() -> Self {
        Self::new(crate::dirs::turns_dir())
    }

    fn turn_dir(&self, turn_id: &str) -> PathBuf {
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        self.base.join(date).join(turn_id)
    }

    fn summary_path(&self, turn_id: &str) -> PathBuf {
        self.turn_dir(turn_id).join("summary.json")
    }

    fn full_path(&self, turn_id: &str) -> PathBuf {
        self.turn_dir(turn_id).join("full.json")
    }

    /// Append `.tmp` to a path for the atomic-write rename target.
    fn tmp_path(path: &Path) -> PathBuf {
        let mut s = path.as_os_str().to_owned();
        s.push(".tmp");
        PathBuf::from(s)
    }

    fn serialize(rec: &TurnRecord) -> Result<String> {
        let mut rec = rec.clone();
        rec.finalize();
        serde_json::to_string_pretty(&rec)
            .map_err(|e| SyscityError::Internal(format!("Failed to serialize turn record: {}", e)))
    }

    async fn atomic_write(path: &Path, contents: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = Self::tmp_path(path);
        tokio::fs::write(&tmp, contents).await?;
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }

    fn atomic_write_blocking(path: &Path, contents: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = Self::tmp_path(path);
        std::fs::write(&tmp, contents)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Write the (finalized) summary record asynchronously.
    pub async fn write(&self, rec: &TurnRecord) -> Result<()> {
        let target = self.summary_path(&rec.turn_id);
        let json = Self::serialize(rec)?;
        Self::atomic_write(&target, &json).await
    }

    /// Append one full-trace event to `full.json` (true append-only).
    ///
    /// Synchronous `std::fs` append, called from the collector's sync
    /// `end_round`/`record_tool` on every event. A turn that crashes before
    /// `finish` therefore still leaves the events emitted so far on disk.
    /// The event is intentionally NOT passed through [`TurnRecord::finalize`],
    /// so request/response/args/result stay untruncated.
    pub fn append_event(&self, turn_id: &str, event: &FullTraceEvent) -> Result<()> {
        use std::io::Write;
        let target = self.full_path(turn_id);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(event).map_err(|e| {
            SyscityError::Internal(format!("Failed to serialize full trace event: {}", e))
        })?;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&target)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    /// Blocking write for contexts without an async runtime (Drop fallback).
    pub fn write_blocking(&self, rec: &TurnRecord) -> Result<()> {
        let target = self.summary_path(&rec.turn_id);
        let json = Self::serialize(rec)?;
        Self::atomic_write_blocking(&target, &json)
    }

    pub fn base(&self) -> &Path {
        &self.base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::record::{FullTraceEvent, ObservedUsage, TurnEndState};
    use tempfile::TempDir;

    fn sample_record(id: &str) -> TurnRecord {
        TurnRecord {
            schema_version: 1,
            turn_id: id.into(),
            session_id: Some("s".into()),
            conversation_id: "c".into(),
            agent_id: "worker".into(),
            thread_id: "main".into(),
            turn_index: 0,
            state: TurnEndState::Complete,
            started_at: "2026-08-14T10:00:00+08:00".into(),
            finished_at: "2026-08-14T10:00:01+08:00".into(),
            duration_ms: 1000,
            ttft_ms: None,
            model: "m".into(),
            user_message_preview: "u".into(),
            assistant_text_preview: "a".into(),
            reasoning_preview: String::new(),
            queue_wait_ms: None,
            cache_hit: false,
            error: None,
            usage: ObservedUsage::default(),
            llm_rounds: vec![],
            tool_calls: vec![],
            route_log: vec![],
            budget_decisions: vec![],
            compressions: vec![],
            plan_snapshot: None,
            channel: None,
        }
    }

    fn date_dir(dir: &TempDir) -> std::path::PathBuf {
        dir.path()
            .join(chrono::Local::now().format("%Y-%m-%d").to_string())
    }

    #[tokio::test]
    async fn write_creates_turn_dir_and_summary() {
        let dir = TempDir::new().unwrap();
        let writer = TurnMetricsWriter::new(dir.path().to_path_buf());
        writer.write(&sample_record("t1")).await.unwrap();

        let file = date_dir(&dir).join("t1").join("summary.json");
        assert!(file.exists());
        let content = std::fs::read_to_string(&file).unwrap();
        let parsed: TurnRecord = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.turn_id, "t1");
        // No tmp file left behind
        assert!(!date_dir(&dir).join("t1").join("summary.json.tmp").exists());
    }

    #[tokio::test]
    async fn concurrent_writes_both_land() {
        let dir = TempDir::new().unwrap();
        let writer = TurnMetricsWriter::new(dir.path().to_path_buf());
        let (r1, r2) = (sample_record("a"), sample_record("b"));
        let (res1, res2) = tokio::join!(writer.write(&r1), writer.write(&r2));
        res1.unwrap();
        res2.unwrap();

        let date = date_dir(&dir);
        assert!(date.join("a").join("summary.json").exists());
        assert!(date.join("b").join("summary.json").exists());
    }

    #[test]
    fn blocking_write_works_without_runtime() {
        let dir = TempDir::new().unwrap();
        let writer = TurnMetricsWriter::new(dir.path().to_path_buf());
        writer.write_blocking(&sample_record("t2")).unwrap();
        assert!(date_dir(&dir).join("t2").join("summary.json").exists());
    }

    #[test]
    fn append_event_appends_jsonl_lines() {
        let dir = TempDir::new().unwrap();
        let writer = TurnMetricsWriter::new(dir.path().to_path_buf());
        let events = vec![
            FullTraceEvent::Round {
                round: 0,
                request: Some(serde_json::json!({"messages": [{"role": "user"}]})),
                response: Some("full output".into()),
                usage: None,
                finish_reason: Some("stop".into()),
                error: None,
            },
            FullTraceEvent::Tool {
                round: 0,
                name: "file_read".into(),
                args: r#"{"path":"/tmp/x"}"#.into(),
                result: "contents".into(),
                success: true,
                duration_ms: 5,
            },
        ];
        for event in &events {
            writer.append_event("t9", event).unwrap();
        }

        let content = std::fs::read_to_string(date_dir(&dir).join("t9").join("full.json")).unwrap();
        let lines: Vec<serde_json::Value> = content
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["type"].as_str().unwrap(), "round");
        assert_eq!(lines[0]["request"]["messages"][0]["role"].as_str().unwrap(), "user");
        assert_eq!(lines[0]["response"].as_str().unwrap(), "full output");
        assert_eq!(lines[1]["type"].as_str().unwrap(), "tool");
        assert_eq!(lines[1]["args"].as_str().unwrap(), r#"{"path":"/tmp/x"}"#);
    }
}
