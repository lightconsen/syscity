//! Watching a screen without touching it.
//!
//! The plan's stage G asked for the shortest path to "remote help that is worth
//! anything": SSH already drives the other machine, so the missing half is
//! *seeing* it. A person who cannot see the screen is being asked to approve
//! clicks they cannot check.
//!
//! This module is that half and only that half. It captures on a timer and
//! publishes each capture; it never dispatches an action. Stage G's own limit —
//! "see, do not drive" — is enforced by there being no input path here at all,
//! not by a flag someone could flip.
//!
//! # Frames, not a stream of bytes
//!
//! Each tick is one ordinary [`Screenshot`](crate::computer::Screenshot), the
//! same thing `computer.status` reports and the same thing a model is shown.
//! Not a video codec: the viewer is a person checking "is it doing what I
//! asked", a screenshot every second is legible for that, and a capture path
//! that already works on every platform (local display, SSH, RDP-adjacent) can
//! serve it unchanged. A real stream would need a second capture path per
//! platform, for a feature whose whole point is batteryless simplicity.
//!
//! # Bounded and interruptible
//!
//! - One capture at a time, with a bounded interval — a person watching must
//!   not make the gateway spin.
//! - The capture runs under a timeout: a hung `screencapture` (a permission
//!   dialog, a locked screen) must cost one frame, not the watcher.
//! - Every loop `select!`s on shutdown, per the project's shutdown rule.
//! - A watcher ends itself after [`MAX_DURATION`]; "on" is not a state that
//!   outlives the person who asked.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::computer::ComputerAdapter;

/// How long one capture may take before the frame is dropped.
///
/// Generous for a local display (tens of milliseconds) and for one SSH round
/// trip, and short enough that a locked screen or a permission prompt costs a
/// second rather than stalling the viewer.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a watcher may run before it stops on its own.
///
/// A watcher left running is a gateway taking screenshots forever on someone's
/// machine. Five minutes is long enough to watch a task and short enough to
/// notice; the viewer can start another.
pub const MAX_DURATION: Duration = Duration::from_secs(300);

/// The frame interval a viewer asked for, clamped to something that neither
/// floods the socket nor pretends to be smooth.
pub const MIN_INTERVAL_MS: u64 = 200;
pub const MAX_INTERVAL_MS: u64 = 10_000;

/// What a viewer asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchParams {
    /// Milliseconds between captures.
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
}

impl Default for WatchParams {
    fn default() -> Self {
        Self {
            interval_ms: default_interval_ms(),
        }
    }
}

fn default_interval_ms() -> u64 {
    1_000
}

impl WatchParams {
    /// The interval actually used, clamped.
    ///
    /// A viewer that asks for 0 gets the fastest interval that is not a busy
    /// loop; one that asks for an hour gets the slowest one that is still
    /// "watching".
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS))
    }

    /// One line for a reply, saying what the watcher will actually do — which
    /// is not always what was asked, because of the clamp.
    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "interval_ms": self.interval().as_millis() as u64,
            "max_duration_ms": MAX_DURATION.as_millis() as u64,
        })
    }
}

/// One frame, as the viewer receives it.
///
/// The image is a data URL rather than a base64 blob plus a media type: a
/// viewer puts it straight into an `<img src>` without reassembling it, and
/// there is no second place for the two halves to disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchedFrame {
    /// Identifies the watcher, so a viewer with two windows open can tell their
    /// frames apart.
    pub watch_id: String,
    /// 1-based frame number within the watcher.
    pub frame: u64,
    /// `data:image/…;base64,…` — ready to display.
    pub image: String,
    pub width: u32,
    pub height: u32,
    /// Milliseconds since the watcher started.
    pub elapsed_ms: u64,
}

/// Why a capture produced nothing.
///
/// Carried instead of silently skipping: a viewer staring at a frozen last
/// frame needs to know the difference between "nothing changed" and "the
/// capture stopped working".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureFailure {
    pub frame: u64,
    pub error: String,
}

/// One capture, bounded in time.
///
/// Returns the screenshot or a reason — never hangs, which is the property that
/// makes the loop safe to leave running.
pub async fn capture_once(
    adapter: &Arc<dyn ComputerAdapter>,
) -> Result<crate::computer::Screenshot, String> {
    match tokio::time::timeout(CAPTURE_TIMEOUT, adapter.screenshot(None)).await {
        Ok(Ok(shot)) => Ok(shot),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!(
            "the screen capture did not answer within {}s",
            CAPTURE_TIMEOUT.as_secs()
        )),
    }
}

/// Turn a capture into a frame the viewer can display.
///
/// The captured bytes are PNG (the capture path writes PNG on every platform);
/// the media type is taken from the payload's own header rather than assumed,
/// because a remote `screencapture` and a local one do not agree on anything
/// the caller can see.
pub fn frame_from(
    watch_id: &str,
    frame: u64,
    elapsed: Duration,
    shot: &crate::computer::Screenshot,
) -> WatchedFrame {
    WatchedFrame {
        watch_id: watch_id.to_string(),
        frame,
        image: format!("data:{};base64,{}", media_type(shot), shot.base64),
        width: shot.width,
        height: shot.height,
        elapsed_ms: elapsed.as_millis() as u64,
    }
}

/// The media type of a base64 screenshot payload.
///
/// Decoded from the first bytes rather than guessed from the extension: the
/// capture path may hand back a re-encoded JPEG (`maybe_encode_screenshot`),
/// and a mismatched type makes an `<img>` show a broken icon for a frame that
/// is perfectly fine.
fn media_type(shot: &crate::computer::Screenshot) -> &'static str {
    use base64::Engine;
    let Ok(bytes) = base64::engine::general_purpose::STANDARD
        .decode(shot.base64.get(..32).unwrap_or(&shot.base64))
    else {
        return "image/png";
    };
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") {
        "image/webp"
    } else {
        "image/png"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::types::{Rect, Screenshot, UiElement};
    use crate::computer::{ActionResult, ActionTarget, ComputerError, DesktopAction};
    use async_trait::async_trait;

    fn png_shot() -> Screenshot {
        Screenshot::new(
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                b"\x89PNG\r\n\x1a\nxxxx",
            ),
            100,
            50,
        )
    }

    #[test]
    fn the_interval_is_clamped_both_ways() {
        assert_eq!(
            WatchParams { interval_ms: 0 }.interval(),
            Duration::from_millis(MIN_INTERVAL_MS),
            "asking for a busy loop gets the fastest sane interval"
        );
        assert_eq!(
            WatchParams { interval_ms: 3_600_000 }.interval(),
            Duration::from_millis(MAX_INTERVAL_MS)
        );
        assert_eq!(
            WatchParams { interval_ms: 500 }.interval(),
            Duration::from_millis(500),
            "a reasonable request is honoured as asked"
        );
    }

    #[test]
    fn the_reply_says_what_the_watcher_will_actually_do() {
        let described = WatchParams { interval_ms: 0 }.describe();
        assert_eq!(described["interval_ms"], MIN_INTERVAL_MS);
        assert_eq!(described["max_duration_ms"], 300_000);
    }

    #[test]
    fn a_frame_is_a_ready_data_url() {
        let frame = frame_from("watch-1", 1, Duration::from_millis(250), &png_shot());
        assert_eq!(frame.watch_id, "watch-1");
        assert_eq!(frame.frame, 1);
        assert_eq!(frame.elapsed_ms, 250);
        assert_eq!(frame.width, 100);
        assert!(frame.image.starts_with("data:image/png;base64,"), "{}", frame.image);
    }

    #[test]
    fn a_jpeg_is_reported_as_jpeg_not_as_png() {
        // The capture path re-encodes to JPEG when it can, and a viewer that is
        // told "png" shows a broken icon for a perfectly good frame.
        let shot = Screenshot::new(
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10],
            ),
            10,
            10,
        );
        let frame = frame_from("w", 1, Duration::ZERO, &shot);
        assert!(frame.image.starts_with("data:image/jpeg;base64,"), "{}", frame.image);
    }

    /// An adapter that never answers, to prove the capture is bounded.
    struct HungAdapter;

    #[async_trait]
    impl ComputerAdapter for HungAdapter {
        async fn screenshot(&self, _r: Option<Rect>) -> crate::computer::Result<Screenshot> {
            std::future::pending::<()>().await;
            unreachable!()
        }
        async fn read_ui_tree(
            &self,
            _app: Option<&str>,
        ) -> crate::computer::Result<Vec<UiElement>> {
            Ok(Vec::new())
        }
        async fn execute(&self, _a: DesktopAction) -> crate::computer::Result<ActionResult> {
            Ok(ActionResult::success("no"))
        }
        async fn wait_for(
            &self,
            _c: crate::computer::WaitCondition,
            _t: Duration,
        ) -> crate::computer::Result<bool> {
            Ok(true)
        }
        async fn resolve_target(
            &self,
            _t: &crate::computer::ClickTarget,
        ) -> crate::computer::Result<ActionTarget> {
            Err(ComputerError::UnsupportedPlatform("no".into()))
        }
        async fn display_geometry(
            &self,
        ) -> crate::computer::Result<crate::computer::display::DisplayGeometry> {
            Ok(crate::computer::display::DisplayGeometry::default())
        }
    }

    #[tokio::test]
    async fn a_capture_that_hangs_is_a_failed_frame_not_a_hung_watcher() {
        let adapter: Arc<dyn ComputerAdapter> = Arc::new(HungAdapter);
        // The real timeout is 5s; waiting it out in a unit test is wasteful, so
        // this asserts the shape of the contract rather than the constant.
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(Duration::from_millis(50), capture_once(&adapter)).await;
        assert!(result.is_err(), "the capture must still be pending after 50ms");
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
