//! Screen recorder for desktop agents — captures video frames for temporal
//! analysis (animation detection, loading state recognition).
//!
//! Uses FFmpeg as the cross-platform capture backend.  Frames are stored in
//! a circular buffer so the agent can analyse recent history without
//! consuming unbounded memory.
//!
//! # Usage
//!
//! ```rust,no_run
//! use std::time::Duration;
//!
//! use syscity::computer::screen_recorder::{RecorderConfig, ScreenRecorder};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let mut recorder = ScreenRecorder::new(RecorderConfig::default())?;
//! recorder.start().await?;
//!
//! // Wait a bit, then check if the scene has stabilised
//! tokio::time::sleep(Duration::from_secs(2)).await;
//! if recorder
//!     .is_scene_stable(Duration::from_millis(500), 1000)
//!     .await
//! {
//!     println!("Loading finished — scene is stable");
//! }
//!
//! recorder.stop().await?;
//! # Ok(())
//! # }
//! ```

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// FFmpeg resolution & auto-download (used before any ffmpeg subprocess call)
// ---------------------------------------------------------------------------

/// Platform-specific data path for the downloaded ffmpeg binary.
fn ffmpeg_cache_path() -> PathBuf {
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let name = if cfg!(target_os = "windows") {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    base.join(".syscity").join("ffmpeg").join(name)
}

/// Check whether `ffmpeg` is already on PATH and works.
async fn check_ffmpeg_on_path() -> Option<PathBuf> {
    tokio::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|_| PathBuf::from("ffmpeg"))
}

/// Resolve the ffmpeg binary: PATH → cache → auto-download.
pub async fn resolve_or_download_ffmpeg() -> crate::computer::Result<PathBuf> {
    // 1. Already on PATH
    if let Some(path) = check_ffmpeg_on_path().await {
        return Ok(path);
    }
    // 2. Already cached
    let cache = ffmpeg_cache_path();
    if cache.exists() {
        let ok = tokio::process::Command::new(&cache)
            .arg("-version")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Ok(cache);
        }
    }
    // 3. Download static build
    info!("ffmpeg not found on PATH — downloading static build to {}", cache.display());
    if let Some(parent) = cache.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            crate::computer::ComputerError::Other(format!(
                "Cannot create cache dir {}: {}",
                parent.display(),
                e
            ))
        })?;
    }
    download_ffmpeg(&cache).await?;
    Ok(cache)
}

/// Download raw bytes from a URL (up to 120 s timeout).
#[cfg(desktop_os)] // only the desktop ffmpeg downloaders call this
async fn download_bytes(url: &str) -> crate::computer::Result<Vec<u8>> {
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent("syscity/0.1")
        .build()
        .map_err(|e| crate::computer::ComputerError::Other(format!("HTTP client: {}", e)))?
        .get(url)
        .send()
        .await
        .map_err(|e| {
            crate::computer::ComputerError::Other(format!("Download failed ({}): {}", url, e))
        })?;
    if !response.status().is_success() {
        return Err(crate::computer::ComputerError::Other(format!(
            "Download returned HTTP {} for {}",
            response.status(),
            url
        )));
    }
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| crate::computer::ComputerError::Other(format!("Download body: {}", e)))
}

/// Download a static ffmpeg binary for the current platform.
#[cfg(target_os = "macos")]
async fn download_ffmpeg(dest: &Path) -> crate::computer::Result<()> {
    use std::io::Read;

    use zip::ZipArchive;

    let url = "https://evermeet.cx/ffmpeg/get/ffmpeg/zip";
    let data = download_bytes(url).await?;

    let cursor = std::io::Cursor::new(data);
    let mut archive = ZipArchive::new(cursor)
        .map_err(|e| crate::computer::ComputerError::Other(format!("Read zip: {}", e)))?;

    let mut extracted = false;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| {
            crate::computer::ComputerError::Other(format!("Zip entry {}: {}", i, e))
        })?;
        let name = entry.name().map_err(|e| {
            crate::computer::ComputerError::Other(format!("Zip entry name {}: {}", i, e))
        })?;
        if name == "ffmpeg" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| {
                crate::computer::ComputerError::Other(format!("Read zip entry: {}", e))
            })?;
            std::fs::write(dest, &buf).map_err(|e| {
                crate::computer::ComputerError::Other(format!("Write {}: {}", dest.display(), e))
            })?;
            extracted = true;
            break;
        }
    }
    if !extracted {
        return Err(crate::computer::ComputerError::Other(
            "ffmpeg binary not found in zip archive".into(),
        ));
    }
    set_executable(dest)?;
    info!("Downloaded ffmpeg to {}", dest.display());
    Ok(())
}

/// Download static ffmpeg for Linux (johnvansickle tar.xz).
#[cfg(target_os = "linux")]
async fn download_ffmpeg(dest: &Path) -> crate::computer::Result<()> {
    let url = "https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-amd64-static.tar.xz";
    let data = download_bytes(url).await?;

    let parent = dest.parent().ok_or_else(|| {
        crate::computer::ComputerError::Other(format!("{} has no parent directory", dest.display()))
    })?;
    let extract_dir = parent.join(".ffmpeg_extract");
    if extract_dir.exists() {
        std::fs::remove_dir_all(&extract_dir).ok();
    }
    std::fs::create_dir_all(&extract_dir).map_err(|e| {
        crate::computer::ComputerError::Other(format!("Create {}: {}", extract_dir.display(), e))
    })?;

    // pipe tar data to `tar -xJf -` (xz support is standard on modern Linux)
    let mut child = tokio::process::Command::new("tar")
        .arg("-xJf")
        .arg("-")
        .arg("-C")
        .arg(&extract_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| crate::computer::ComputerError::Other(format!("spawn tar: {}", e)))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&data).await.map_err(|e| {
            crate::computer::ComputerError::Other(format!("write to tar stdin: {}", e))
        })?;
    }

    let status = child
        .wait()
        .await
        .map_err(|e| crate::computer::ComputerError::Other(format!("wait tar: {}", e)))?;
    if !status.success() {
        return Err(crate::computer::ComputerError::Other(
            "tar extraction failed (is xz-utils installed?)".into(),
        ));
    }

    // Find the ffmpeg binary inside the extracted directory tree
    let found = find_file_recursively(&extract_dir, "ffmpeg").ok_or_else(|| {
        crate::computer::ComputerError::Other("ffmpeg binary not found in extracted archive".into())
    })?;
    set_executable(&found)?;
    std::fs::rename(&found, dest)
        .map_err(|e| crate::computer::ComputerError::Other(format!("move ffmpeg: {}", e)))?;
    std::fs::remove_dir_all(&extract_dir).ok();
    info!("Downloaded ffmpeg to {}", dest.display());
    Ok(())
}

/// Download static ffmpeg for Windows (gyan.dev essentials zip).
#[cfg(target_os = "windows")]
async fn download_ffmpeg(dest: &Path) -> crate::computer::Result<()> {
    use std::io::Read;

    use zip::ZipArchive;

    let url = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";
    let data = download_bytes(url).await?;

    let parent = dest.parent().ok_or_else(|| {
        crate::computer::ComputerError::Other(format!("{} has no parent directory", dest.display()))
    })?;
    let extract_dir = parent.join(".ffmpeg_extract");
    if extract_dir.exists() {
        std::fs::remove_dir_all(&extract_dir).ok();
    }
    std::fs::create_dir_all(&extract_dir).map_err(|e| {
        crate::computer::ComputerError::Other(format!("Create {}: {}", extract_dir.display(), e))
    })?;

    let cursor = std::io::Cursor::new(data);
    let mut archive = ZipArchive::new(cursor)
        .map_err(|e| crate::computer::ComputerError::Other(format!("Read zip: {}", e)))?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| {
            crate::computer::ComputerError::Other(format!("Zip entry {}: {}", i, e))
        })?;
        // zip 9 returns `Result<Cow<str>>` here; a name that is not valid
        // UTF-8 is a real failure, not something to lossily stringify.
        let name = entry
            .name()
            .map_err(|e| {
                crate::computer::ComputerError::Other(format!("Zip entry name {}: {}", i, e))
            })?
            .to_string();
        if entry.is_dir() {
            std::fs::create_dir_all(extract_dir.join(&name)).ok();
        } else {
            let out = extract_dir.join(&name);
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p).ok();
            }
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| {
                crate::computer::ComputerError::Other(format!("Read zip entry: {}", e))
            })?;
            std::fs::write(&out, &buf).map_err(|e| {
                crate::computer::ComputerError::Other(format!("Write {}: {}", out.display(), e))
            })?;
        }
    }

    // Find ffmpeg.exe in the extracted tree
    let found = find_file_recursively(&extract_dir, "ffmpeg.exe").ok_or_else(|| {
        crate::computer::ComputerError::Other("ffmpeg.exe not found in extracted archive".into())
    })?;
    std::fs::rename(&found, dest)
        .map_err(|e| crate::computer::ComputerError::Other(format!("move ffmpeg.exe: {}", e)))?;
    std::fs::remove_dir_all(&extract_dir).ok();
    info!("Downloaded ffmpeg to {}", dest.display());
    Ok(())
}

#[cfg(not(desktop_os))]
async fn download_ffmpeg(_dest: &Path) -> crate::computer::Result<()> {
    Err(crate::computer::ComputerError::Other(
        "Automatic ffmpeg download not supported on this platform. Please install ffmpeg manually."
            .into(),
    ))
}

// Desktop unixes (linux/macos) chmod the downloaded ffmpeg; Windows and
// mobile use the no-op. `desktop_os` alone would misfire on Windows, plain
// `unix` would misfire on Android/iOS — hence the intersection.
#[cfg(all(unix, desktop_os))]
fn set_executable(path: &Path) -> crate::computer::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|e| {
        crate::computer::ComputerError::Other(format!("stat {}: {}", path.display(), e))
    })?;
    let mut perms = meta.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(|e| {
        crate::computer::ComputerError::Other(format!("chmod {}: {}", path.display(), e))
    })
}

#[cfg(all(not(unix), desktop_os))] // Windows only; mobile has no caller
fn set_executable(_path: &Path) -> crate::computer::Result<()> {
    Ok(())
}

/// Recursively search for a file by name in a directory tree.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn find_file_recursively(dir: &Path, target: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file_recursively(&path, target) {
                return Some(found);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(target) {
            return Some(path);
        }
    }
    None
}

/// A single captured video frame.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Capture timestamp.
    pub timestamp: Instant,
    /// Raw RGBA pixel data (width × height × 4 bytes).
    pub data: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
}

/// Configuration for the screen recorder.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecorderConfig {
    /// Target capture frames per second.
    pub fps: u32,
    /// Maximum duration to keep in the circular buffer (seconds).
    pub max_buffer_secs: u64,
    /// Output resolution width (0 = native).
    pub output_width: u32,
    /// Output resolution height (0 = native).
    pub output_height: u32,
    /// Region to capture (`None` = full screen).
    pub region: Option<Rect>,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        Self {
            fps: 5,
            max_buffer_secs: 10,
            output_width: 0,
            output_height: 0,
            region: None,
        }
    }
}

/// Capture region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Where captured frames go besides the circular buffer.
///
/// The buffer is a *window* — `max_buffer_secs` of recent frames, for asking
/// "is the scene stable". A recording wants the whole run, which is unbounded
/// and must not be held in memory. So a sink is handed every frame as it
/// arrives and writes it out; the buffer keeps its own window alongside.
///
/// `Send`-safe and called from the capture task, so an implementation must not
/// block: pushing onto an mpsc is fine, awaiting ffmpeg is not.
pub type StreamSink = Arc<dyn Fn(VideoFrame) + Send + Sync>;

/// Screen recorder backed by FFmpeg.
pub struct ScreenRecorder {
    config: RecorderConfig,
    frame_buffer: Arc<Mutex<VecDeque<VideoFrame>>>,
    running: Arc<AtomicBool>,
    capture_task: Option<tokio::task::JoinHandle<()>>,
    save_path: Option<PathBuf>,
    stream_sink: Option<StreamSink>,
}

impl ScreenRecorder {
    /// Create a new recorder with the given configuration.
    pub fn new(config: RecorderConfig) -> crate::computer::Result<Self> {
        Ok(Self {
            config,
            frame_buffer: Arc::new(Mutex::new(VecDeque::new())),
            running: Arc::new(AtomicBool::new(false)),
            capture_task: None,
            save_path: None,
            stream_sink: None,
        })
    }

    /// Start capturing frames in the background.
    pub async fn start(&mut self) -> crate::computer::Result<()> {
        if self.running.load(Ordering::SeqCst) {
            return Err(crate::computer::ComputerError::Other(
                "Recorder already running".to_string(),
            ));
        }

        let ffmpeg_bin = resolve_or_download_ffmpeg().await?;

        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let buffer = self.frame_buffer.clone();
        let config = self.config;
        let sink = self.stream_sink.clone();

        let handle = tokio::spawn(async move {
            if let Err(e) = run_ffmpeg_capture(config, buffer, running, ffmpeg_bin, sink).await {
                warn!("Screen recorder capture loop exited: {}", e);
            }
        });

        self.capture_task = Some(handle);
        info!("Screen recorder started at {} fps", self.config.fps);
        Ok(())
    }

    /// Stop capturing and optionally save the buffer to a video file.
    pub async fn stop(&mut self) -> crate::computer::Result<()> {
        self.running.store(false, Ordering::SeqCst);

        if let Some(handle) = self.capture_task.take() {
            if tokio::time::timeout(Duration::from_secs(2), handle)
                .await
                .is_err()
            {
                warn!("Screen recorder capture task did not stop within 2s timeout");
            }
        }

        if let Some(path) = &self.save_path {
            self.save_buffer_to_video(path).await?;
        }

        info!("Screen recorder stopped");
        Ok(())
    }

    /// Set a path to save the recorded video when stopping.
    pub fn set_save_path<P: AsRef<Path>>(&mut self, path: P) {
        self.save_path = Some(path.as_ref().to_path_buf());
    }

    /// Get frames captured within the last `duration`.
    pub async fn recent_frames(&self, duration: Duration) -> Vec<VideoFrame> {
        let buffer = self.frame_buffer.lock().await;
        let cutoff = Instant::now() - duration;
        buffer
            .iter()
            .filter(|f| f.timestamp >= cutoff)
            .cloned()
            .collect()
    }

    /// Check whether the scene has been "stable" for the given window.
    ///
    /// Stability is defined as: the pixel-wise difference between every
    /// consecutive pair of frames in the window is below
    /// `pixel_diff_threshold`.
    pub async fn is_scene_stable(&self, window: Duration, pixel_diff_threshold: u32) -> bool {
        let frames = self.recent_frames(window).await;
        if frames.len() < 2 {
            return false; // Not enough history to decide.
        }

        for pair in frames.windows(2) {
            let diff = pixel_diff(&pair[0], &pair[1]);
            if diff > pixel_diff_threshold {
                return false;
            }
        }
        true
    }

    /// Get the most recent frame, if any.
    pub async fn latest_frame(&self) -> Option<VideoFrame> {
        let buffer = self.frame_buffer.lock().await;
        buffer.back().cloned()
    }

    /// Compute the pixel difference between the two most recent frames.
    pub async fn latest_frame_diff(&self) -> Option<u32> {
        let buffer = self.frame_buffer.lock().await;
        let frames: Vec<_> = buffer.iter().rev().take(2).collect();
        if frames.len() < 2 {
            return None;
        }
        Some(pixel_diff(frames[1], frames[0]))
    }

    /// Send every captured frame to `sink` as well as into the buffer.
    ///
    /// Set before [`Self::start`]: the sink is moved into the capture task, and
    /// a sink installed after the task exists would never be called.
    pub fn with_stream_sink(mut self, sink: StreamSink) -> Self {
        self.stream_sink = Some(sink);
        self
    }

    /// Inject a synthetic frame into the buffer (for testing).
    pub async fn inject_frame(&self, frame: VideoFrame) {
        self.frame_buffer.lock().await.push_back(frame);
    }

    /// Save the current frame buffer as an MP4 video using FFmpeg.
    async fn save_buffer_to_video(&self, path: &Path) -> crate::computer::Result<()> {
        // Drain all frames from the buffer
        let frames: Vec<VideoFrame> = {
            let mut buf = self.frame_buffer.lock().await;
            buf.drain(..).collect()
        };

        if frames.is_empty() {
            info!("No frames to save — buffer is empty");
            return Ok(());
        }

        let ffmpeg_bin = resolve_or_download_ffmpeg().await?;
        let width = frames[0].width;
        let height = frames[0].height;
        let fps = self.config.fps;
        let total = frames.len();

        // Spawn ffmpeg to encode raw RGBA frames into H.264 MP4.
        let mut child = tokio::process::Command::new(&ffmpeg_bin)
            .arg("-y")
            .arg("-f").arg("rawvideo")
            .arg("-pix_fmt").arg("rgba")
            .arg("-s").arg(format!("{}x{}", width, height))
            .arg("-r").arg(fps.to_string())
            .arg("-i").arg("-") // read from stdin
            .arg("-c:v").arg("libx264")
            .arg("-pix_fmt").arg("yuv420p")
            .arg("-preset").arg("fast")
            .arg(path)
            .stdin(Stdio::piped())
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .map_err(|e| {
                crate::computer::ComputerError::Other(format!(
                    "Failed to spawn ffmpeg for encoding: {}",
                    e
                ))
            })?;

        let mut stdin = child.stdin.take().ok_or_else(|| {
            crate::computer::ComputerError::Other("Failed to open ffmpeg stdin".to_string())
        })?;

        for frame in &frames {
            stdin.write_all(&frame.data).await.map_err(|e| {
                crate::computer::ComputerError::Other(format!(
                    "Failed to write frame to ffmpeg: {}",
                    e
                ))
            })?;
        }

        // Close stdin so ffmpeg knows the input is complete
        drop(stdin);

        let status = child.wait().await.map_err(|e| {
            crate::computer::ComputerError::Other(format!("Failed to wait for ffmpeg: {}", e))
        })?;

        if !status.success() {
            warn!("ffmpeg encoding exited with status: {}", status);
            return Err(crate::computer::ComputerError::Other(format!(
                "ffmpeg encoding failed with status: {}",
                status
            )));
        }

        info!(
            "Saved {} frames ({}x{}, {} fps) to {}",
            total,
            width,
            height,
            fps,
            path.display()
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FFmpeg capture backend
// ---------------------------------------------------------------------------

async fn run_ffmpeg_capture(
    config: RecorderConfig,
    buffer: Arc<Mutex<VecDeque<VideoFrame>>>,
    running: Arc<AtomicBool>,
    ffmpeg_bin: PathBuf,
    sink: Option<StreamSink>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut cmd = build_ffmpeg_command(config, &ffmpeg_bin)?;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().ok_or("ffmpeg has no stdout")?;
    let stderr = child.stderr.take();

    // Read raw RGBA frames from FFmpeg stdout.
    let mut reader = tokio::io::BufReader::new(stdout);

    // The frame size is what ffmpeg was told to *produce*, which is not always
    // the screen: a region capture delivers region-sized frames, and reading
    // full-screen-sized ones off that stream never completes a frame. This is
    // the same rule `build_ffmpeg_command` applies to its input, stated once.
    let (width, height) = match capture_size(&config) {
        Some((w, h)) => (w, h),
        None => match detect_screen_resolution().await {
            Some(res) => res,
            None => {
                warn!("Could not detect screen resolution, falling back to 1920x1080");
                (1920, 1080)
            }
        },
    };

    let actual_frame_size = (width * height * 4) as usize;
    let max_frames = (config.fps as u64 * config.max_buffer_secs) as usize;

    let mut frame_data = vec![0u8; actual_frame_size];

    while running.load(Ordering::SeqCst) {
        match tokio::time::timeout(Duration::from_secs(2), reader.read_exact(&mut frame_data)).await
        {
            Ok(Ok(_)) => {
                let data = std::mem::take(&mut frame_data);
                frame_data = vec![0u8; actual_frame_size];
                let frame = VideoFrame {
                    timestamp: Instant::now(),
                    data,
                    width,
                    height,
                };

                accept_frame(frame, &sink, &buffer, max_frames).await;
            }
            Ok(Err(e)) => {
                warn!("FFmpeg stdout read error: {}", e);
                break;
            }
            Err(_) => {
                // Timeout — check if we should still be running.
                continue;
            }
        }
    }

    if let Err(e) = child.kill().await {
        warn!("Failed to kill ffmpeg child: {}", e);
    }
    if let Err(e) = child.wait().await {
        warn!("Failed to wait for ffmpeg child exit: {}", e);
    }

    // Log ffmpeg stderr for diagnostics (e.g. codec init failures).
    if let Some(mut stderr) = stderr {
        use tokio::io::AsyncReadExt;
        let mut buf = String::new();
        if stderr.read_to_string(&mut buf).await.unwrap_or(0) > 0 {
            warn!("ffmpeg stderr output: {}", buf.trim());
        }
    }

    Ok(())
}

/// What one captured frame does: the recording sink first, then the buffer.
///
/// A named step rather than five lines inline because the *order* is the
/// contract — a recording must not lose a frame to the buffer's window — and
/// the only way to test an order inside a capture loop is to call the step
/// that implements it.
async fn accept_frame(
    frame: VideoFrame,
    sink: &Option<StreamSink>,
    buffer: &Mutex<VecDeque<VideoFrame>>,
    max_frames: usize,
) {
    // The sink sees the frame first: it is the recording, and a frame dropped
    // to keep the buffer small must still be written.
    if let Some(sink) = sink {
        sink(frame.clone());
    }
    let mut buf = buffer.lock().await;
    buf.push_back(frame);
    while buf.len() > max_frames {
        buf.pop_front();
    }
}

/// The frame size the capture will actually deliver, when it is known.
///
/// A region comes first because it is the one dimension ffmpeg is *told* to
/// produce (`-video_size` on X11/Windows, a `crop` filter on macOS) and
/// nothing downstream rescales it. `output_width`/`output_height` are the
/// requested size and win only in the absence of a region; `None` means "the
/// screen's own size, which only the platform can say".
fn capture_size(config: &RecorderConfig) -> Option<(u32, u32)> {
    if let Some(r) = config.region {
        if r.width != 0 && r.height != 0 {
            return Some((r.width, r.height));
        }
    }
    if config.output_width != 0 && config.output_height != 0 {
        return Some((config.output_width, config.output_height));
    }
    None
}

fn build_ffmpeg_command(
    config: RecorderConfig,
    ffmpeg_bin: &Path,
) -> Result<tokio::process::Command, Box<dyn std::error::Error + Send + Sync>> {
    let mut cmd = tokio::process::Command::new(ffmpeg_bin);
    cmd.arg("-y"); // Overwrite output

    // Input format and device depend on platform.
    #[cfg(target_os = "linux")]
    {
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0.0".to_string());
        cmd.arg("-f").arg("x11grab");
        if let Some(r) = config.region {
            cmd.arg("-video_size")
                .arg(format!("{}x{}", r.width, r.height))
                .arg("-i")
                .arg(format!("{}+{}, {}", display, r.x, r.y));
        } else {
            cmd.arg("-i").arg(display);
        }
    }

    #[cfg(target_os = "macos")]
    {
        cmd.arg("-f").arg("avfoundation");
        if let Some(r) = config.region {
            // `avfoundation` has no offset option of its own: `-video_size` on
            // the input would ask the *device* for a size it does not support,
            // not crop it. So the device is captured whole and the region is
            // taken from it with a `crop` filter. Dropping the crop is what
            // used to record the top-left corner while reporting the requested
            // region — a recording of the wrong part of the screen is worse
            // than no recording, because it looks like the right one.
            cmd.arg("-i").arg("1:");
            cmd.arg("-vf")
                .arg(format!("crop={}:{}:{}:{}", r.width, r.height, r.x, r.y));
        } else {
            cmd.arg("-i").arg("1:");
        }
    }

    #[cfg(target_os = "windows")]
    {
        cmd.arg("-f").arg("gdigrab");
        if let Some(r) = config.region {
            cmd.arg("-video_size")
                .arg(format!("{}x{}", r.width, r.height))
                .arg("-offset_x")
                .arg(r.x.to_string())
                .arg("-offset_y")
                .arg(r.y.to_string())
                .arg("-i")
                .arg("desktop");
        } else {
            cmd.arg("-i").arg("desktop");
        }
    }

    // Framerate.
    cmd.arg("-framerate").arg(config.fps.to_string());

    // Output raw RGBA to stdout.
    cmd.arg("-pix_fmt").arg("rgba")
        .arg("-f").arg("rawvideo")
        .arg("-an") // No audio
        .arg("-"); // stdout

    Ok(cmd)
}

async fn detect_screen_resolution() -> Option<(u32, u32)> {
    #[cfg(target_os = "linux")]
    {
        // Try xrandr or xdpyinfo.
        if let Ok(output) = tokio::process::Command::new("xrandr")
            .arg("--current")
            .output()
            .await
        {
            let text = String::from_utf8_lossy(&output.stdout);
            let re = regex::Regex::new(r"(\d+)x(\d+)\+").ok()?;
            for line in text.lines() {
                // Look for "*+" marking the current mode.
                if line.contains("*+") {
                    if let Some(caps) = re.captures(line) {
                        let w = caps.get(1)?.as_str().parse().ok()?;
                        let h = caps.get(2)?.as_str().parse().ok()?;
                        return Some((w, h));
                    }
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = tokio::process::Command::new("system_profiler")
            .args(["SPDisplaysDataType", "-json"])
            .output()
            .await
        {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(displays) = json.get("SPDisplaysDataType")?.as_array() {
                    if let Some(first) = displays.first() {
                        let w = first
                            .get("_spdisplays_pixels")?
                            .as_str()?
                            .split('x')
                            .next()?
                            .parse()
                            .ok()?;
                        let h = first
                            .get("_spdisplays_pixels")?
                            .as_str()?
                            .split('x')
                            .nth(1)?
                            .parse()
                            .ok()?;
                        return Some((w, h));
                    }
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(output) = tokio::process::Command::new("powershell")
            .arg("-Command")
            .arg(
                "Add-Type -AssemblyName System.Windows.Forms; \
                 [System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Size",
            )
            .output()
            .await
        {
            let text = String::from_utf8_lossy(&output.stdout);
            let re = regex::Regex::new(r"Width\s*=\s*(\d+).*Height\s*=\s*(\d+)").ok()?;
            if let Some(caps) = re.captures(&text) {
                let w = caps.get(1)?.as_str().parse().ok()?;
                let h = caps.get(2)?.as_str().parse().ok()?;
                return Some((w, h));
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Streaming encoder
// ---------------------------------------------------------------------------

/// Writes frames to an MP4 as they arrive.
///
/// [`ScreenRecorder::set_save_path`] keeps the frames and encodes them all at
/// [`ScreenRecorder::stop`], which is right for a short window but wrong for a
/// recording: a person may record for minutes, and holding every RGBA frame
/// would be gigabytes (1920×1080×4 is ~8 MB a frame; at 5 fps that is 40 MB a
/// second). A recording therefore encodes continuously — one ffmpeg process,
/// fed frame by frame — so what it holds is a pipe, not a buffer.
///
/// The encoder is a `tokio::process::Child` writing to stdin, not the blocking
/// `std::process::Command` the buffer path uses: this one runs inside the
/// capture task, where awaiting is allowed and blocking is not.
pub struct StreamingEncoder {
    child: tokio::process::Child,
    /// `Some` until [`Self::finish`] closes it: closing the pipe is what tells
    /// ffmpeg the recording ended, so it cannot simply be a field.
    stdin: Option<tokio::process::ChildStdin>,
    width: u32,
    height: u32,
    fps: u32,
    path: PathBuf,
    frames: u64,
}

impl StreamingEncoder {
    /// Start an encoder that will write the recording to `path`.
    ///
    /// The dimensions are not known until ffmpeg's capture has read its first
    /// frame, so ffmpeg is told to read `rgba` at the size the *frame*
    /// reports — see [`Self::start_with_size`]. The size is fixed for the
    /// duration: a display that changes resolution mid-recording is not a case
    /// worth a second encoder.
    pub async fn start(
        path: impl Into<PathBuf>,
        width: u32,
        height: u32,
        fps: u32,
    ) -> crate::computer::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                crate::computer::ComputerError::Other(format!(
                    "Cannot create {} for the recording: {e}",
                    parent.display()
                ))
            })?;
        }
        let ffmpeg_bin = resolve_or_download_ffmpeg().await?;

        let mut child = tokio::process::Command::new(&ffmpeg_bin)
            .arg("-y")
            .arg("-f")
            .arg("rawvideo")
            .arg("-pix_fmt")
            .arg("rgba")
            .arg("-s")
            .arg(format!("{width}x{height}"))
            .arg("-r")
            .arg(fps.to_string())
            .arg("-i")
            .arg("-")
            .arg("-c:v")
            .arg("libx264")
            .arg("-pix_fmt")
            .arg("yuv420p")
            .arg("-preset")
            .arg("veryfast")
            .arg(&path)
            .stdin(Stdio::piped())
            // Inherited rather than piped: a piped stderr that nobody drains
            // fills its buffer and blocks ffmpeg mid-recording.
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .map_err(|e| {
                crate::computer::ComputerError::Other(format!(
                    "Failed to start the recording encoder: {e}"
                ))
            })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            crate::computer::ComputerError::Other("the recording encoder has no stdin".to_string())
        })?;

        info!("Recording to {} ({width}x{height} @ {fps} fps)", path.display());
        Ok(Self {
            child,
            stdin: Some(stdin),
            width,
            height,
            fps,
            path,
            frames: 0,
        })
    }

    /// The path the recording is being written to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many frames have been written.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Write one frame. A frame whose size does not match is dropped, with a
    /// warning: handing ffmpeg a short buffer would corrupt the whole file.
    pub async fn write(&mut self, frame: &VideoFrame) -> crate::computer::Result<()> {
        if frame.width != self.width || frame.height != self.height {
            warn!(
                "dropping a {}x{} frame; the recording is {}x{}",
                frame.width, frame.height, self.width, self.height
            );
            return Ok(());
        }
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(crate::computer::ComputerError::Other(
                "the recording has already been closed".to_string(),
            ));
        };
        stdin
            .write_all(&frame.data)
            .await
            .map_err(|e| crate::computer::ComputerError::Other(format!("writing a frame: {e}")))?;
        self.frames += 1;
        Ok(())
    }

    /// Close the recording and report where it is.
    ///
    /// Closing stdin is the signal ffmpeg waits for, so the final file is only
    /// complete after `wait()` returns. A non-zero exit is an error even so:
    /// the file may exist and be truncated, and reporting a path to it as a
    /// success would be a lie a player would discover.
    pub async fn finish(mut self) -> crate::computer::Result<PathBuf> {
        // Dropping the pipe is the signal ffmpeg waits for.
        drop(self.stdin.take());

        let status = self.child.wait().await.map_err(|e| {
            crate::computer::ComputerError::Other(format!("waiting for the encoder: {e}"))
        })?;
        if !status.success() {
            return Err(crate::computer::ComputerError::Other(format!(
                "the recorder exited {status} after {} frames; {} is incomplete",
                self.frames,
                self.path.display()
            )));
        }
        info!(
            "Recording finished: {} frames ({}x{} @ {} fps) → {}",
            self.frames,
            self.width,
            self.height,
            self.fps,
            self.path.display()
        );
        Ok(self.path)
    }
}

// ---------------------------------------------------------------------------
// Frame analysis
// ---------------------------------------------------------------------------

/// Compute pixel-wise absolute difference between two frames.
/// Returns the number of pixels that differ by more than a small threshold.
fn pixel_diff(a: &VideoFrame, b: &VideoFrame) -> u32 {
    if a.width != b.width || a.height != b.height || a.data.len() != b.data.len() {
        // Different sizes → treat as completely different.
        return a.width * a.height;
    }

    // Sample every 16th pixel for performance (sparse diff).
    let stride = 16usize.saturating_mul(4);
    let mut diff_count = 0u32;

    for i in (0..a.data.len()).step_by(stride) {
        let end = (i + 4).min(a.data.len());
        let pa = &a.data[i..end];
        let pb = &b.data[i..end];
        let pixel_diff: u32 = pa
            .iter()
            .zip(pb.iter())
            .map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs())
            .sum();
        if pixel_diff > 30 {
            diff_count += 1;
        }
    }

    // Scale back up from sparse sampling.
    diff_count * (stride as u32 / 4)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_frame(width: u32, height: u32, fill: u8) -> VideoFrame {
        let size = (width * height * 4) as usize;
        VideoFrame {
            timestamp: Instant::now(),
            data: vec![fill; size],
            width,
            height,
        }
    }

    #[test]
    fn test_pixel_diff_identical() {
        let a = make_test_frame(100, 100, 128);
        let b = make_test_frame(100, 100, 128);
        assert_eq!(pixel_diff(&a, &b), 0);
    }

    #[test]
    fn test_pixel_diff_different() {
        let a = make_test_frame(100, 100, 0);
        let b = make_test_frame(100, 100, 255);
        let diff = pixel_diff(&a, &b);
        assert!(diff > 0);
        // Sparse sampling means diff_count * sampling_factor.
        assert!(diff <= 100 * 100);
    }

    #[test]
    fn test_pixel_diff_size_mismatch() {
        let a = make_test_frame(100, 100, 0);
        let b = make_test_frame(200, 100, 0);
        assert_eq!(pixel_diff(&a, &b), 100 * 100);
    }

    /// A region capture must read region-sized frames off ffmpeg. Sizing the
    /// read against the *screen* leaves the reader waiting for bytes that never
    /// arrive: the capture then records nothing at all.
    #[test]
    fn a_region_capture_sizes_its_frames_to_the_region() {
        let config = RecorderConfig {
            region: Some(Rect {
                x: 100,
                y: 50,
                width: 640,
                height: 480,
            }),
            ..Default::default()
        };
        assert_eq!(capture_size(&config), Some((640, 480)));
    }

    /// An explicit output size wins when there is no region to capture.
    #[test]
    fn an_explicit_output_size_is_used_without_a_region() {
        let config = RecorderConfig {
            output_width: 800,
            output_height: 600,
            ..Default::default()
        };
        assert_eq!(capture_size(&config), Some((800, 600)));
    }

    /// Nothing known: the platform has to be asked, and the caller can tell
    /// that apart from "zero-sized frames".
    #[test]
    fn an_unconfigured_capture_has_no_size_of_its_own() {
        assert_eq!(capture_size(&RecorderConfig::default()), None);
    }

    /// The command line says where the region is, on every platform. A macOS
    /// region used to be *silently* dropped — it produced a recording of the
    /// top-left corner under the requested region's name.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_region_is_cropped_out_of_the_full_frame() {
        let config = RecorderConfig {
            region: Some(Rect {
                x: 120,
                y: 60,
                width: 640,
                height: 480,
            }),
            ..Default::default()
        };
        let cmd = build_ffmpeg_command(config, Path::new("ffmpeg")).expect("command");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let joined = args.join(" ");
        assert!(
            joined.contains("crop=640:480:120:60"),
            "the requested region must be cropped, not recorded from the origin: {joined}"
        );
        assert!(
            !joined.contains("-video_size"),
            "a device-level size is not a crop: ffmpeg rejects it: {joined}"
        );
    }

    /// A frame must reach the sink even when the buffer would drop it.
    ///
    /// The buffer is a window (`max_buffer_secs`); a recording is the whole
    /// run. Tying the two together would make a long recording silently lose
    /// its beginning — exactly the part a person wants to review.
    #[tokio::test]
    async fn a_streamed_frame_reaches_the_sink_even_when_the_buffer_drops_it() {
        use std::sync::Mutex as StdMutex;

        let seen: Arc<StdMutex<Vec<u32>>> = Arc::new(StdMutex::new(Vec::new()));
        let seen_in_sink = seen.clone();
        let sink: Option<StreamSink> = Some(Arc::new(move |frame: VideoFrame| {
            seen_in_sink.lock().expect("not poisoned").push(frame.width);
        }));
        // A buffer one frame wide: of five frames, four are dropped from it.
        let buffer = Mutex::new(VecDeque::new());

        for i in 0..5u32 {
            // Alternate widths so the sink's record says *which* frames came
            // through, not only how many.
            accept_frame(make_test_frame(64 + i, 48, 7), &sink, &buffer, 1).await;
        }

        let widths = seen.lock().expect("not poisoned").clone();
        assert_eq!(
            widths,
            vec![64, 65, 66, 67, 68],
            "every captured frame must reach the recording sink, in order"
        );
        // The window did its own job: it kept only the last frame.
        let buf = buffer.lock().await;
        assert_eq!(buf.len(), 1, "the buffer is still bounded");
        assert_eq!(buf[0].width, 68, "and it holds the newest frame");
    }

    #[tokio::test]
    async fn test_recorder_buffer_management() {
        let recorder = ScreenRecorder::new(RecorderConfig {
            fps: 10,
            max_buffer_secs: 1,
            output_width: 10,
            output_height: 10,
            region: None,
        })
        .unwrap();

        // Manually inject frames, simulating what the capture loop does
        // (push + cap at max_frames).
        {
            let mut buf = recorder.frame_buffer.lock().await;
            let max_frames =
                (recorder.config.fps as u64 * recorder.config.max_buffer_secs) as usize;
            for i in 0..20 {
                buf.push_back(make_test_frame(10, 10, i as u8));
                while buf.len() > max_frames {
                    buf.pop_front();
                }
            }
        }

        // Buffer should not exceed max_frames = fps * max_buffer_secs = 10.
        let buf = recorder.frame_buffer.lock().await;
        assert!(buf.len() <= 10, "buffer should be capped at max_frames");
    }

    #[tokio::test]
    async fn test_save_buffer_to_video_empty() {
        let recorder = ScreenRecorder::new(RecorderConfig::default()).unwrap();
        // Empty buffer should not error
        recorder
            .save_buffer_to_video(Path::new("/tmp/empty_test.mp4"))
            .await
            .unwrap();
    }
}
