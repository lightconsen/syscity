//! The video side of a recording: one encoder per `computer.record_start`.
//!
//! Recording a workflow already captures the *actions* (a `Workflow` of
//! `Click`/`Type`/… that can be replayed). This module adds the picture
//! alongside them — an MP4 of what the screen showed while the actions were
//! taken — which is what a person needs when the replay later does the wrong
//! thing and the question is "what did it look like when I recorded this".
//!
//! Three moving parts, each with a reason to exist:
//!
//! - [`ScreenRecorder`] captures raw frames. Its buffer is a *window*
//!   (`max_buffer_secs`), not the whole recording, so a sink is installed to
//!   hand each frame onward as it arrives.
//! - [`StreamingEncoder`] is that sink's destination: one ffmpeg process fed
//!   frame by frame. Holding a ten-minute recording in memory is not an option
//!   (about 40 MB a second at 1920×1080, 5 fps), and the buffer's
//!   stop-then-encode path is built for seconds, not minutes.
//! - The *supervisor* below polls the recording and, when it ends, closes the
//!   encoder and moves the file into the artifacts directory.
//!
//! The supervisor polls [`Recording::session_id`] rather than being told about
//! `record_stop`. That is deliberate: `record_stop` is one caller among several
//! ways a recording ends (the same handle is dropped when the gateway shuts
//! down), and a video that only stops when someone remembers to ring a bell is
//! a video that is truncated whenever they forget. Watching the session directly
//! means the encoder ends when the recording does, whoever ended it.
//!
//! Off by the default: `[computer] record_video = true` opts in. Screen
//! recording needs a permission the rest of computer use does not (macOS Screen
//! Recording), and a feature that starts a process per recording should be
//! asked for rather than assumed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::computer::screen_recorder::{RecorderConfig, ScreenRecorder, StreamingEncoder};
use crate::computer::VideoFrame;

/// How often the supervisor asks whether the recording is still going.
///
/// Fast enough that a short recording is not mostly missing, slow enough that
/// the poll is not a load. One atomic read and a mutex lock — microseconds.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How long to wait for the first frame before giving up on capturing.
///
/// A recording over a display nobody is permitted to read produces no frames at
/// all; without a deadline the supervisor would sit there for as long as the
/// recording ran, holding a task and saying nothing.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// Capture rate. Five is the default the recorder uses, and enough for a
/// screen recording whose purpose is "show me what happened".
const FPS: u32 = 5;

/// The file name a recording's video is written under.
///
/// `<sanitised name>-<session>.mp4` — the session id keeps two recordings under
/// the same name from overwriting each other, which is exactly what happens
/// when someone records twice while exploring.
pub fn video_file_name(name: Option<&str>, session: u64) -> String {
    let stem: String = name
        .unwrap_or("recording")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let stem = stem.trim_matches('-').to_string();
    let stem = if stem.is_empty() {
        "recording".to_string()
    } else {
        stem
    };
    format!("{stem}-{session}.mp4")
}

/// Why a capture could not start.
#[derive(Debug)]
pub enum CaptureError {
    /// The host has no ffmpeg and it could not be downloaded.
    NoEncoder(String),
    /// The screen could not be read: no permission, or no display.
    ///
    /// Carried separately from `NoEncoder` because the fix is different — a
    /// permission, not an install — and the caller's message should say which.
    NoFrames(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoEncoder(e) => write!(f, "no screen encoder is available: {e}"),
            Self::NoFrames(e) => write!(f, "the screen could not be read: {e}"),
        }
    }
}

/// Whether this host can plausibly capture a screen right now.
///
/// Deliberately shallow: it answers "is there an ffmpeg binary", not "will the
/// capture work". A real capture is the only honest test of the latter, and
/// this check exists so `record_start` can say "no" immediately instead of
/// spawning a supervisor that will fail five seconds later.
pub async fn encoder_available() -> bool {
    crate::computer::screen_recorder::resolve_or_download_ffmpeg()
        .await
        .is_ok()
}

/// Start capturing the screen for a recording, and stop when the recording
/// does.
///
/// Returns the path the video is being written to *before* it is complete: the
/// caller is starting a recording, not waiting for one, and a path that only
/// appears at the end is a path nobody can show progress against.
///
/// One task owns the whole capture. Tying it to the recording handle here rather
/// than in the caller means there is exactly one answer to "when does the video
/// stop" — when the recording it belongs to stops, whoever stopped it — and no
/// second place that can forget to.
pub async fn start_capture(
    recording: Arc<crate::planner::workflow::Recording>,
    session: u64,
    name: Option<String>,
    dir: PathBuf,
) -> Result<Capture, CaptureError> {
    if !encoder_available().await {
        return Err(CaptureError::NoEncoder(
            "ffmpeg is not installed and could not be downloaded".to_string(),
        ));
    }

    let path = dir.join(video_file_name(name.as_deref(), session));
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(CaptureError::NoEncoder(format!(
                "cannot create {}: {e}",
                parent.display()
            )));
        }
    }

    // The frame channel: the sink pushes, the capture task pulls. Bounded so a
    // display that outruns the encoder applies backpressure to the sink rather
    // than growing an unbounded queue of 8 MB frames.
    let (tx, rx) = mpsc::channel::<VideoFrame>(8);

    let recorder = ScreenRecorder::new(RecorderConfig { fps: FPS, ..Default::default() })
        .map_err(|e| CaptureError::NoEncoder(e.to_string()))?
        .with_stream_sink(Arc::new(move |frame: VideoFrame| {
            // `try_send` rather than awaiting: this runs on the capture task, and
            // blocking there stalls frame reads. A dropped frame is better than a
            // stalled capture.
            let _ = tx.try_send(frame);
        }));

    let capture = CaptureCreator {
        recorder,
        encoder: None,
        rx,
        path: path.clone(),
        recording,
        session,
    };
    // Detached on purpose: the task ends when the recording does, and nobody
    // waits for a recording to finish before returning. The handle goes back to
    // the caller so the gateway can register it — this module must not depend
    // on `gateway`, and a task nobody holds is a task a shutdown cannot stop.
    let task = tokio::spawn(capture.run());

    Ok(Capture { path, task })
}

/// A capture that is running.
pub struct Capture {
    /// The file being written. Incomplete until the task finishes.
    pub path: PathBuf,
    /// The capture's task, for a caller that wants to register or stop it.
    pub task: tokio::task::JoinHandle<()>,
}

/// Everything one capture owns, driven in a single task.
///
/// A creator rather than a supervisor function because the encoder cannot exist
/// until the first frame tells it its dimensions, and once the encoder and the
/// recorder are both live the loop is the same shape either way.
pub struct CaptureCreator {
    recorder: ScreenRecorder,
    encoder: Option<StreamingEncoder>,
    rx: mpsc::Receiver<VideoFrame>,
    path: PathBuf,
    recording: Arc<crate::planner::workflow::Recording>,
    session: u64,
}

impl CaptureCreator {
    /// Capture frames until the recording ends, then finish the file.
    async fn run(mut self) {
        if let Err(e) = self.recorder.start().await {
            warn!("could not start capturing the screen for the recording: {e}");
            return;
        }

        let first = tokio::time::timeout(FIRST_FRAME_TIMEOUT, self.rx.recv()).await;
        let Some(first) = (match first {
            Ok(frame) => frame,
            Err(_elapsed) => {
                warn!(
                    "no frames arrived within {:?}; {} will have no video. Screen Recording \
                     permission, or a display to capture, is missing",
                    FIRST_FRAME_TIMEOUT,
                    self.path.display()
                );
                let _ = self.recorder.stop().await;
                return;
            }
        }) else {
            let _ = self.recorder.stop().await;
            return;
        };

        let (width, height) = (first.width, first.height);
        let encoder = match StreamingEncoder::start(&self.path, width, height, FPS).await {
            Ok(encoder) => encoder,
            Err(e) => {
                warn!("could not start the recording encoder: {e}");
                let _ = self.recorder.stop().await;
                return;
            }
        };
        // Write the first frame before storing the encoder, so the two cannot
        // disagree about whether it exists.
        let mut encoder = encoder;
        if let Err(e) = encoder.write(&first).await {
            warn!("writing the first recorded frame failed: {e}");
        }
        self.encoder = Some(encoder);

        // Two ways this ends: the frame channel closes, or the recording the
        // video belongs to stops. The second is the one that always happens —
        // the sink's sender lives in the capture task, so the channel alone
        // would only close after the recording was already gone.
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                frame = self.rx.recv() => {
                    let Some(frame) = frame else { break };
                    if let Some(encoder) = self.encoder.as_mut() {
                        if let Err(e) = encoder.write(&frame).await {
                            warn!("the recording encoder stopped accepting frames: {e}");
                            break;
                        }
                    }
                }
                _ = ticker.tick() => {
                    if self.recording.session_id() != Some(self.session) {
                        break;
                    }
                }
            }
        }

        if let Err(e) = self.recorder.stop().await {
            warn!("the screen recorder did not stop cleanly: {e}");
        }

        if let Some(encoder) = self.encoder.take() {
            let frames = encoder.frames();
            match encoder.finish().await {
                Ok(path) => info!("recording video written: {} ({frames} frames)", path.display()),
                Err(e) => warn!("the recording video is incomplete: {e}"),
            }
        }
    }
}

/// Where a recording's video lives.
pub fn video_dir(paths: &crate::dirs::SyscityPaths) -> PathBuf {
    paths.artifacts_dir().join("computer-recordings")
}

/// The video path a workflow row stores, relative to `dir` when it is inside
/// it. Stored relative so a `~/.syscity` that is moved keeps working.
pub fn relative_to(dir: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(dir)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn frame(w: u32, h: u32, fill: u8) -> VideoFrame {
        VideoFrame {
            timestamp: Instant::now(),
            data: vec![fill; (w * h * 4) as usize],
            width: w,
            height: h,
        }
    }

    /// The file name has to survive a name a person typed. A recording called
    /// `Check /etc & "builds"` must not produce a path with directories in it.
    #[test]
    fn a_video_name_is_safe_for_any_recording_name() {
        let name = video_file_name(Some(r#"Check /etc & "builds""#), 3);
        assert_eq!(name, "Check--etc----builds-3.mp4");
        assert!(!name.contains('/'), "{name}");

        // No name at all still has to be a file name.
        assert_eq!(video_file_name(None, 1), "recording-1.mp4");
        // A name made only of separators must not become empty.
        assert_eq!(video_file_name(Some("///"), 2), "recording-2.mp4");
    }

    /// Two recordings under the same name must not overwrite each other.
    #[test]
    fn the_session_id_keeps_two_recordings_apart() {
        assert_ne!(video_file_name(Some("setup"), 1), video_file_name(Some("setup"), 2));
    }

    /// A path inside the artifact directory is stored relative; one outside it
    /// is stored as given, so a moved root is not silently mangled.
    #[test]
    fn a_stored_path_is_relative_only_when_it_should_be() {
        let dir = Path::new("/home/u/.syscity/artifacts/computer-recordings");
        assert_eq!(relative_to(dir, &dir.join("setup-1.mp4")).as_deref(), Some("setup-1.mp4"));
        assert_eq!(relative_to(dir, Path::new("/elsewhere/x.mp4")), None);
    }
}
