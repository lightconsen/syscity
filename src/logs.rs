//! Log management for Syscity
//!
//! Provides functionality to view and tail daemon logs.

// INVARIANTS-NONE: log tailing client; owns no state
use std::path::PathBuf;

use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader, SeekFrom};

/// Get the default log file path
pub fn log_file_path() -> PathBuf {
    crate::dirs::default_log_file()
}

/// Show last N lines of logs
pub async fn show_logs(n: usize) -> crate::Result<()> {
    let log_path = log_file_path();

    if !log_path.exists() {
        println!("ℹ️ No log file found at {:?}", log_path);
        println!("   Logs will be created when the daemon starts.");
        return Ok(());
    }

    let file = File::open(&log_path)
        .await
        .map_err(crate::error::SyscityError::Io)?;

    let reader = BufReader::new(file);
    let mut lines = reader.lines();
    let mut all_lines: Vec<String> = Vec::new();

    // Read all lines
    while let Some(line) = lines.next_line().await? {
        all_lines.push(line);
    }

    // Print last N lines
    let start = if all_lines.len() > n {
        all_lines.len() - n
    } else {
        0
    };
    for line in &all_lines[start..] {
        println!("{}", line);
    }

    Ok(())
}

/// Tail logs (follow mode like tail -f)
pub async fn tail_logs(n: usize) -> crate::Result<()> {
    let log_path = log_file_path();

    if !log_path.exists() {
        println!("ℹ️ No log file found at {:?}", log_path);
        println!("   Logs will be created when the daemon starts.");
        return Ok(());
    }

    println!("📜 Tailing logs (press Ctrl+C to stop)...\n");

    // First show last N lines
    show_logs(n).await?;

    let file = File::open(&log_path)
        .await
        .map_err(crate::error::SyscityError::Io)?;

    let mut reader = BufReader::new(file);

    // Seek to end to start tailing
    let file_len = reader.seek(SeekFrom::End(0)).await?;
    let mut pos = file_len;

    // Handle SIGTERM (Unix only; Windows has no signals — Ctrl+C is handled
    // by the ctrl_c arm below, and this future then stays pending forever).
    #[cfg(unix)]
    let mut sigterm: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> = {
        let mut stream = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        Box::pin(async move {
            stream.recv().await;
        })
    };
    #[cfg(not(unix))]
    let mut sigterm: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
        Box::pin(std::future::pending());

    loop {
        tokio::select! {
            // Check for new lines
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
                let file = File::open(&log_path).await?;
                let new_len = file.metadata().await?.len();

                if new_len > pos {
                    // New content added
                    let mut new_reader = BufReader::new(file);
                    new_reader.seek(SeekFrom::Start(pos)).await?;

                    let mut lines = new_reader.lines();
                    while let Some(line) = lines.next_line().await? {
                        println!("{}", line);
                    }

                    pos = new_len;
                } else if new_len < pos {
                    // File was truncated/rotated, seek to beginning
                    pos = 0;
                }
            }
            // Handle SIGTERM (pending forever when unavailable)
            _ = &mut sigterm => {
                println!("\n👋 Stopped tailing logs");
                break;
            }
            // Handle Ctrl+C
            _ = tokio::signal::ctrl_c() => {
                println!("\n👋 Stopped tailing logs");
                break;
            }
        }
    }

    Ok(())
}

/// Log writer for daemon
pub struct LogWriter {
    file: Option<tokio::fs::File>,
    path: PathBuf,
}

impl LogWriter {
    /// Create a new log writer
    pub async fn new() -> crate::Result<Self> {
        let path = log_file_path();

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Open file in append mode
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(crate::error::SyscityError::Io)?;

        Ok(Self { file: Some(file), path })
    }

    /// Write a log line
    pub async fn write(&mut self, line: &str) -> crate::Result<()> {
        use tokio::io::AsyncWriteExt;

        if let Some(ref mut file) = self.file {
            file.write_all(line.as_bytes()).await?;
            file.write_all(b"\n").await?;
            file.flush().await?;
        }
        Ok(())
    }

    /// Get the log file path
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_file_path() {
        let path = log_file_path();
        assert!(path.to_string_lossy().contains("syscity"));
    }

    #[tokio::test]
    async fn test_log_writer_create_and_write() {
        let temp_dir = tempfile::tempdir().unwrap();
        let log_path = temp_dir.path().join("test.log");

        let mut writer = LogWriter {
            file: Some(
                tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .await
                    .unwrap(),
            ),
            path: log_path.clone(),
        };

        assert_eq!(writer.path(), &log_path);

        writer.write("Test line 1").await.unwrap();
        writer.write("Test line 2").await.unwrap();

        // Verify file contents
        let content = tokio::fs::read_to_string(&log_path).await.unwrap();
        assert!(content.contains("Test line 1"));
        assert!(content.contains("Test line 2"));
    }

    #[tokio::test]
    async fn test_log_writer_write_no_file() {
        let mut writer = LogWriter {
            file: None,
            path: PathBuf::from("/tmp/test.log"),
        };

        // Should not panic, just return Ok
        let result = writer.write("test").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_show_logs_empty_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let log_path = temp_dir.path().join("empty.log");
        tokio::fs::write(&log_path, "").await.unwrap();

        // show_logs requires log_file_path() which uses default log path.
        // We can't easily mock it, so just verify it doesn't panic on missing
        // file. Since log_file_path() returns a fixed path, this test
        // is limited.
    }
}
