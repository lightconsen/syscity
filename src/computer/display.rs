//! Display geometry and the screenshot ↔ screen coordinate contract.
//!
//! A screenshot and a click do not necessarily share a coordinate space. On a
//! Retina Mac a 1800×1169-point screen captures as a 3600×2338-pixel image, and
//! the screenshot encoder may resize further on top of that. A model that reads
//! "the button is at (2400, 800)" from the image and clicks there lands 1200
//! points too far right.
//!
//! This module makes the mapping explicit instead of leaving it to chance:
//!
//! - [`DisplayGeometry`] — every display's frame and name, in the same
//!   top-left-origin global points that click coordinates use.
//! - [`CoordinateSpace`] — the conversion for one screenshot: image pixels →
//!   screen points, with the image's origin anchored at the display union's
//!   origin so multi-monitor captures work too.
//!
//! Detection shells out to the platform's own display API (AppKit on macOS,
//! `System.Windows.Forms.Screen` on Windows, `xrandr` on Linux). It is cached
//! for [`GEOMETRY_TTL`] because a display layout only changes on user action,
//! and probing costs a process spawn.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::types::{Rect, Screenshot};

/// How long a detected display layout is trusted.
pub const GEOMETRY_TTL: Duration = Duration::from_secs(60);

/// How long a display probe may take before it is abandoned.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// One display, in top-left-origin global screen points.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Whether this is the OS's main display.
    #[serde(default)]
    pub primary: bool,
    /// Human-readable name, when the platform reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl DisplayInfo {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
            primary: false,
            name: None,
        }
    }

    pub fn primary(mut self) -> Self {
        self.primary = true;
        self
    }

    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// This display's frame as a rectangle.
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
}

/// The layout of every display attached to the machine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayGeometry {
    pub displays: Vec<DisplayInfo>,
}

impl DisplayGeometry {
    pub fn new(displays: Vec<DisplayInfo>) -> Self {
        Self { displays }
    }

    /// Whether detection produced anything. An empty geometry means "unknown",
    /// not "no displays"; callers must refuse to convert rather than guess 1:1.
    pub fn is_empty(&self) -> bool {
        self.displays.is_empty()
    }

    /// The display the OS treats as primary, else the first one reported.
    pub fn primary(&self) -> Option<&DisplayInfo> {
        self.displays
            .iter()
            .find(|d| d.primary)
            .or_else(|| self.displays.first())
    }

    /// Bounding box of all displays — the area a full-screen capture covers.
    pub fn union(&self) -> Option<Rect> {
        let first = self.displays.first()?;
        let mut min_x = first.x;
        let mut min_y = first.y;
        let mut max_x = first.x.saturating_add(first.width as i32);
        let mut max_y = first.y.saturating_add(first.height as i32);
        for display in &self.displays[1..] {
            min_x = min_x.min(display.x);
            min_y = min_y.min(display.y);
            max_x = max_x.max(display.x.saturating_add(display.width as i32));
            max_y = max_y.max(display.y.saturating_add(display.height as i32));
        }
        Some(Rect {
            x: min_x,
            y: min_y,
            width: (max_x - min_x).max(0) as u32,
            height: (max_y - min_y).max(0) as u32,
        })
    }

    /// Render a display list for the model, one display per line.
    pub fn summary(&self) -> String {
        if self.displays.is_empty() {
            return "display geometry unavailable".to_string();
        }
        self.displays
            .iter()
            .enumerate()
            .map(|(i, d)| {
                format!(
                    "display {i}{} at ({},{}) {}x{} pts{}",
                    if d.primary { " (primary)" } else { "" },
                    d.x,
                    d.y,
                    d.width,
                    d.height,
                    d.name
                        .as_ref()
                        .map(|n| format!(" {n:?}"))
                        .unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// The mapping from one screenshot's pixels to screen points.
///
/// The image is assumed to cover the union of the displays, with its top-left
/// corner at the union's origin — which is what a full-screen capture does on
/// every platform we drive. A region capture must not be described by this
/// type; it needs its own origin.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CoordinateSpace {
    /// Screenshot width in pixels.
    pub image_width: u32,
    /// Screenshot height in pixels.
    pub image_height: u32,
    /// Global screen-point origin of the captured area.
    pub screen_x: i32,
    pub screen_y: i32,
    /// Size of the captured area in screen points.
    pub screen_width: u32,
    pub screen_height: u32,
}

impl CoordinateSpace {
    /// Build the mapping for an image of `image_width`×`image_height` covering
    /// the whole display layout.
    pub fn new(image_width: u32, image_height: u32, geometry: &DisplayGeometry) -> Option<Self> {
        if image_width == 0 || image_height == 0 {
            return None;
        }
        let union = geometry.union()?;
        if union.width == 0 || union.height == 0 {
            return None;
        }
        Some(Self {
            image_width,
            image_height,
            screen_x: union.x,
            screen_y: union.y,
            screen_width: union.width,
            screen_height: union.height,
        })
    }

    /// Build the mapping for a screenshot that was just taken.
    pub fn from_screenshot(screenshot: &Screenshot, geometry: &DisplayGeometry) -> Option<Self> {
        Self::new(screenshot.width, screenshot.height, geometry)
    }

    /// Each image pixel advances this many screen points horizontally.
    pub fn scale_x(&self) -> f64 {
        self.screen_width as f64 / self.image_width as f64
    }

    /// Each image pixel advances this many screen points vertically.
    pub fn scale_y(&self) -> f64 {
        self.screen_height as f64 / self.image_height as f64
    }

    /// Whether image pixels and screen points coincide.
    pub fn is_identity(&self) -> bool {
        self.image_width == self.screen_width
            && self.image_height == self.screen_height
            && self.screen_x == 0
            && self.screen_y == 0
    }

    /// Convert a point in screenshot pixels to screen points.
    pub fn image_to_screen(&self, x: i32, y: i32) -> (i32, i32) {
        let sx = self.screen_x as f64 + x as f64 * self.scale_x();
        let sy = self.screen_y as f64 + y as f64 * self.scale_y();
        (sx.round() as i32, sy.round() as i32)
    }

    /// Convert a point in screen points to screenshot pixels.
    pub fn screen_to_image(&self, x: i32, y: i32) -> (i32, i32) {
        let ix = (x as f64 - self.screen_x as f64) / self.scale_x();
        let iy = (y as f64 - self.screen_y as f64) / self.scale_y();
        (ix.round() as i32, iy.round() as i32)
    }

    /// One-line description for tool output.
    pub fn summary(&self) -> String {
        format!(
            "screenshot {}x{} px maps to screen {}x{} pts at ({},{}) — scale {:.2}x{:.2}",
            self.image_width,
            self.image_height,
            self.screen_width,
            self.screen_height,
            self.screen_x,
            self.screen_y,
            self.scale_x(),
            self.scale_y()
        )
    }
}

// ── Detection ───────────────────────────────────────────────────────────────

static CACHE: OnceLock<Mutex<Option<(Instant, DisplayGeometry)>>> = OnceLock::new();

fn cache() -> &'static Mutex<Option<(Instant, DisplayGeometry)>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Detected layout, reused for [`GEOMETRY_TTL`].
pub async fn detect() -> DisplayGeometry {
    if let Ok(guard) = cache().lock() {
        if let Some((at, geometry)) = guard.as_ref() {
            if at.elapsed() < GEOMETRY_TTL {
                return geometry.clone();
            }
        }
    }
    let geometry = detect_uncached().await;
    // Never hold the lock across the probe: it spawns processes and awaits.
    if let Ok(mut guard) = cache().lock() {
        *guard = Some((Instant::now(), geometry.clone()));
    }
    geometry
}

/// Force a fresh probe, bypassing and replacing the cache.
pub async fn detect_uncached() -> DisplayGeometry {
    #[cfg(target_os = "macos")]
    let geometry = probe_macos().await;
    #[cfg(target_os = "windows")]
    let geometry = probe_windows().await;
    #[cfg(target_os = "linux")]
    let geometry = probe_linux().await;
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let geometry = DisplayGeometry::default();
    geometry
}

async fn run_probe(program: &str, args: &[&str]) -> Option<String> {
    let output = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::process::Command::new(program).args(args).output(),
    )
    .await;
    match output {
        Ok(Ok(out)) if out.status.success() => {
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        Ok(Ok(out)) => {
            tracing::debug!(
                "display probe {program} exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            None
        }
        Ok(Err(e)) => {
            tracing::debug!("display probe {program} failed to start: {e}");
            None
        }
        Err(_) => {
            tracing::debug!("display probe {program} timed out");
            None
        }
    }
}

/// JXA one-liner: every screen's frame, converted from AppKit's
/// bottom-left-origin global space into the top-left-origin space that
/// CGEvent clicks and accessibility bounds use.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MACOS_SCREEN_SCRIPT: &str = r#"
ObjC.import('AppKit');
var screens = $.NSScreen.screens.js;
if (screens.length === 0) throw new Error('no screens');
var primaryHeight = screens[0].frame.size.height;
screens.map(function (s, i) {
  var f = s.frame;
  return JSON.stringify({
    x: Math.round(f.origin.x),
    y: Math.round(primaryHeight - f.origin.y - f.size.height),
    w: Math.round(f.size.width),
    h: Math.round(f.size.height),
    primary: i === 0,
    name: ObjC.unwrap(s.localizedName)
  });
}).join('\n');
"#;

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
async fn probe_macos() -> DisplayGeometry {
    match run_probe("osascript", &["-l", "JavaScript", "-e", MACOS_SCREEN_SCRIPT]).await {
        Some(out) => parse_macos_screens(&out),
        None => DisplayGeometry::default(),
    }
}

/// Parse the line-delimited JSON emitted by [`MACOS_SCREEN_SCRIPT`].
///
/// Only the caller is cfg-gated, so the tests below can cover the parser on
/// any host.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_macos_screens(output: &str) -> DisplayGeometry {
    let mut displays = Vec::new();
    for line in output.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(display) = display_from_json(&value) {
                displays.push(display);
            }
        }
    }
    DisplayGeometry::new(displays)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn display_from_json(value: &serde_json::Value) -> Option<DisplayInfo> {
    let x = value.get("x")?.as_i64()? as i32;
    let y = value.get("y")?.as_i64()? as i32;
    let width = value.get("w")?.as_u64()? as u32;
    let height = value.get("h")?.as_u64()? as u32;
    let mut display = DisplayInfo::new(x, y, width, height);
    display.primary = value
        .get("primary")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    display.name = value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|n| !n.is_empty())
        .map(String::from);
    Some(display)
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const WINDOWS_SCREEN_SCRIPT: &str = r#"
Add-Type -AssemblyName System.Windows.Forms
[System.Windows.Forms.Screen]::AllScreens | ForEach-Object {
  '{0}|{1}|{2}|{3}|{4}|{5}' -f $_.Bounds.X, $_.Bounds.Y, $_.Bounds.Width, $_.Bounds.Height, $_.Primary, $_.DeviceName
}
"#;

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
async fn probe_windows() -> DisplayGeometry {
    match run_probe(
        "powershell",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            WINDOWS_SCREEN_SCRIPT,
        ],
    )
    .await
    {
        Some(out) => parse_windows_screens(&out),
        None => DisplayGeometry::default(),
    }
}

/// Parse `x|y|w|h|primary|name` lines from `System.Windows.Forms.Screen`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn parse_windows_screens(output: &str) -> DisplayGeometry {
    let mut displays = Vec::new();
    for line in output.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let fields: Vec<&str> = line.split('|').collect();
        if fields.len() < 5 {
            continue;
        }
        let parsed = (
            fields[0].parse::<i32>(),
            fields[1].parse::<i32>(),
            fields[2].parse::<u32>(),
            fields[3].parse::<u32>(),
        );
        if let (Ok(x), Ok(y), Ok(width), Ok(height)) = parsed {
            let mut display = DisplayInfo::new(x, y, width, height);
            display.primary = fields[4].eq_ignore_ascii_case("true");
            display.name = fields
                .get(5)
                .map(|n| n.trim())
                .filter(|n| !n.is_empty())
                .map(String::from);
            displays.push(display);
        }
    }
    DisplayGeometry::new(displays)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
async fn probe_linux() -> DisplayGeometry {
    if let Some(out) = run_probe("xrandr", &["--query"]).await {
        let geometry = parse_xrandr_query(&out);
        if !geometry.is_empty() {
            return geometry;
        }
    }
    // Fallback: Xvfb and minimal installations often ship xdpyinfo only.
    if let Some(out) = run_probe("xdpyinfo", &[]).await {
        return parse_xdpyinfo(&out);
    }
    DisplayGeometry::default()
}

/// Parse `xrandr --query` output: connected outputs with a `WxH+X+Y` geometry.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_xrandr_query(output: &str) -> DisplayGeometry {
    let mut displays = Vec::new();
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else { continue };
        if !line.contains(" connected") {
            continue;
        }
        let rest: Vec<&str> = fields.collect();
        let primary = rest.contains(&"primary");
        // The first token that parses as geometry is the current mode.
        for field in rest {
            if let Some(display) = parse_x11_geometry(field) {
                let mut display = display;
                display.primary = primary;
                display.name = Some(name.to_string());
                displays.push(display);
                break;
            }
        }
    }
    DisplayGeometry::new(displays)
}

/// Parse `1920x1080+0+0` into a display frame.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_x11_geometry(text: &str) -> Option<DisplayInfo> {
    let mut parts = text.split('+');
    let size = parts.next()?;
    let x = parts.next()?.parse::<i32>().ok()?;
    let y = parts.next()?.parse::<i32>().ok()?;
    let (width, height) = size.split_once('x')?;
    let width = width.parse::<u32>().ok()?;
    let height = height.parse::<u32>().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some(DisplayInfo::new(x, y, width, height))
}

/// Parse the single `dimensions: WxH pixels` line from `xdpyinfo`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_xdpyinfo(output: &str) -> DisplayGeometry {
    for line in output.lines() {
        if let Some(rest) = line.trim().strip_prefix("dimensions:") {
            if let Some(token) = rest.split_whitespace().next() {
                if let Some((w, h)) = token.split_once('x') {
                    if let (Ok(width), Ok(height)) = (w.parse::<u32>(), h.parse::<u32>()) {
                        return DisplayGeometry::new(vec![
                            DisplayInfo::new(0, 0, width, height).primary()
                        ]);
                    }
                }
            }
        }
    }
    DisplayGeometry::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retina_single() -> DisplayGeometry {
        DisplayGeometry::new(vec![DisplayInfo::new(0, 0, 1800, 1169).primary()])
    }

    #[test]
    fn retina_image_maps_to_points() {
        let space = CoordinateSpace::new(3600, 2338, &retina_single()).unwrap();
        assert_eq!(space.scale_x(), 0.5);
        assert_eq!(space.image_to_screen(2400, 1600), (1200, 800));
        assert_eq!(space.screen_to_image(1200, 800), (2400, 1600));
        assert!(!space.is_identity());
    }

    #[test]
    fn identity_when_capture_is_unscaled() {
        let geometry = DisplayGeometry::new(vec![DisplayInfo::new(0, 0, 1920, 1080).primary()]);
        let space = CoordinateSpace::new(1920, 1080, &geometry).unwrap();
        assert!(space.is_identity());
        assert_eq!(space.image_to_screen(100, 200), (100, 200));
    }

    #[test]
    fn multi_display_union_anchors_the_image() {
        // A 1800x1169 laptop with a 2560x1440 monitor to its left, both
        // captured into one 8720x2338-pixel image (2x on the laptop side).
        let geometry = DisplayGeometry::new(vec![
            DisplayInfo::new(0, 0, 1800, 1169)
                .primary()
                .named("Built-in"),
            DisplayInfo::new(-1280, 0, 1280, 720).named("External"),
        ]);
        let union = geometry.union().unwrap();
        assert_eq!((union.x, union.y), (-1280, 0));
        assert_eq!((union.width, union.height), (3080, 1169));

        let space = CoordinateSpace::new(6160, 2338, &geometry).unwrap();
        assert_eq!(space.scale_x(), 0.5);
        // The laptop's top-left sits at image x = (0 - (-1280)) / 0.5 = 2560.
        assert_eq!(space.image_to_screen(2560, 0), (0, 0));
        // The external display's top-left is the image origin.
        assert_eq!(space.image_to_screen(0, 0), (-1280, 0));
    }

    #[test]
    fn unknown_geometry_never_guesses() {
        assert!(CoordinateSpace::new(100, 100, &DisplayGeometry::default()).is_none());
        assert!(CoordinateSpace::new(0, 0, &retina_single()).is_none());
    }

    #[test]
    fn from_screenshot_uses_the_encoded_size() {
        let shot = Screenshot::new(String::new(), 3600, 2338);
        let space = CoordinateSpace::from_screenshot(&shot, &retina_single()).unwrap();
        assert_eq!((space.image_width, space.image_height), (3600, 2338));
    }

    #[test]
    fn summary_names_both_spaces() {
        let space = CoordinateSpace::new(3600, 2338, &retina_single()).unwrap();
        let summary = space.summary();
        assert!(summary.contains("3600x2338"), "{summary}");
        assert!(summary.contains("1800x1169"), "{summary}");
        assert!(summary.contains("0.50"), "{summary}");
    }

    #[test]
    fn parses_macos_screen_output() {
        let output = r#"{"x":0,"y":0,"w":1800,"h":1169,"primary":true,"name":"Built-in Retina Display"}
{"x":1800,"y":-200,"w":2560,"h":1440,"primary":false,"name":"DELL U2720Q"}"#;
        let geometry = parse_macos_screens(output);
        assert_eq!(geometry.displays.len(), 2);
        assert!(geometry.displays[0].primary);
        assert_eq!(geometry.displays[1].x, 1800);
        assert_eq!(geometry.displays[1].y, -200);
        assert_eq!(geometry.displays[1].name.as_deref(), Some("DELL U2720Q"));
    }

    #[test]
    fn ignores_malformed_macos_lines() {
        let geometry = parse_macos_screens("not json\n{\"x\":1}\n");
        assert!(geometry.is_empty());
    }

    #[test]
    fn parses_windows_screen_output() {
        let output =
            "0|0|1920|1080|True|\\\\.\\DISPLAY1\n-2560|0|2560|1440|False|\\\\.\\DISPLAY2\n";
        let geometry = parse_windows_screens(output);
        assert_eq!(geometry.displays.len(), 2);
        assert!(geometry.displays[0].primary);
        assert_eq!(geometry.displays[1].x, -2560);
        assert_eq!(geometry.displays[1].height, 1440);
    }

    #[test]
    fn parses_xrandr_query() {
        let output = "\
Screen 0: minimum 8 x 8, current 4480 x 1440, maximum 32767 x 32767
eDP-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 344mm x 193mm
   1920x1080     60.00*+
HDMI-1 connected 2560x1440+1920+0 (normal left inverted right x axis y axis) 597mm x 336mm
DP-1 disconnected (normal left inverted right x axis y axis)
";
        let geometry = parse_xrandr_query(output);
        assert_eq!(geometry.displays.len(), 2);
        assert!(geometry.displays[0].primary);
        assert_eq!(geometry.displays[0].name.as_deref(), Some("eDP-1"));
        assert_eq!(geometry.displays[1].x, 1920);
        assert_eq!(geometry.displays[1].width, 2560);
    }

    #[test]
    fn parses_xdpyinfo_fallback() {
        let output = "name of display:    :99\nversion number:    11.0\n  dimensions:    1280x720 pixels (338x190 millimeters)\n";
        let geometry = parse_xdpyinfo(output);
        assert_eq!(geometry.displays.len(), 1);
        assert_eq!(geometry.displays[0].width, 1280);
        assert!(geometry.displays[0].primary);
    }

    #[tokio::test]
    async fn detection_is_cached_and_well_formed() {
        let first = detect().await;
        let second = detect().await;
        assert_eq!(first, second);
        // On a machine with a display every entry must be a real rectangle.
        for display in &first.displays {
            assert!(display.width > 0 && display.height > 0);
        }
    }
}
