//! Running another program, for the device sets that drive one.
//!
//! A device set controls something outside this process — a phone over ADB, an
//! iPhone over `idevice*`, a VM over HTTP — and almost all of that control ends
//! up as "run this binary and read what it said". Every set needs the same
//! three answers from that: did it finish, what did it print, and how do I get
//! the bytes rather than a `String` when the output is a PNG.
//!
//! Routing through [`crate::tools::process_runner`] rather than calling
//! `tokio::process::Command` directly is what makes the *same* call correct on
//! a desktop and inside the APK: on Android the runner resolves the bundled
//! `adb` out of `nativeLibraryDir` and refuses executables SELinux would block,
//! on iOS it refuses to fork at all, and on a desktop it is the ordinary spawn.
//! A set that spawns its own child bypasses all three.

use crate::tools::process_runner::{ProcessError, ProcessRequest};

/// Check whether a command answers on PATH.
///
/// `--version` is the probe because every tool these sets drive accepts it and
/// exits before doing anything. A tool that needs a different probe gets its
/// own check rather than a parameter here.
pub fn has_command(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Shared helper: run a command and return (success, stdout, stderr).
///
/// Spawn errors surface as the underlying `io::Error`; there is no timeout.
/// Anything that can hang must state its bound, which is [`run_cmd_timeout`].
pub async fn run_cmd(
    cmd: &str,
    args: &[&str],
) -> std::io::Result<(std::process::ExitStatus, String, String)> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(cmd.to_string());
    argv.extend(args.iter().map(|s| s.to_string()));
    let req = ProcessRequest::argv(&argv.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    run_cmd_with(&req).await
}

/// [`run_cmd`] with a wall-clock bound.
///
/// [`ProcessRequest::timeout`] defaults to `None`, which waits forever; the
/// device lock a caller holds is held for exactly as long as the process runs,
/// so anything that can hang must state its bound here. On expiry the child is
/// killed and this reports a timeout rather than an `io::Error`: "it did not
/// answer in time" is a result the caller can explain, not a transport fault.
pub async fn run_cmd_timeout(cmd: &str, args: &[&str], timeout: std::time::Duration) -> RunOutcome {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(cmd.to_string());
    argv.extend(args.iter().map(|s| s.to_string()));
    let mut req = ProcessRequest::argv(&argv.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    req.timeout = Some(timeout);

    match crate::tools::process_runner::run_collect(&req).await {
        Ok(out) if out.timed_out => RunOutcome::TimedOut {
            stdout: out.stdout_string(),
            stderr: out.stderr_string(),
        },
        Ok(out) => match out.status {
            Some(status) => RunOutcome::Finished {
                status,
                stdout: out.stdout_string(),
                stderr: out.stderr_string(),
            },
            None => RunOutcome::TimedOut {
                stdout: out.stdout_string(),
                stderr: out.stderr_string(),
            },
        },
        Err(e) => RunOutcome::Failed(e.to_string()),
    }
}

/// What [`run_cmd_timeout`] observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Finished {
        status: std::process::ExitStatus,
        stdout: String,
        stderr: String,
    },
    /// The child was killed at the deadline. Whatever it printed first is kept.
    TimedOut { stdout: String, stderr: String },
    /// It never ran (spawn failure, bad argv).
    Failed(String),
}

/// [`run_cmd`] for a request that carries more than argv — stdin, notably.
///
/// The display helper's launch sends a one-time nonce on stdin, and that is the
/// only way the app and the helper agree on it without putting it somewhere
/// another shell process can read.
pub async fn run_cmd_with(
    req: &ProcessRequest,
) -> std::io::Result<(std::process::ExitStatus, String, String)> {
    let out = crate::tools::process_runner::run(req).await.map_err(|e| {
        // Keep the spawn failure as the underlying `io::Error` so callers can
        // wrap it in `ExternalService` and keep the OS's message.
        match e {
            ProcessError::Spawn { source, .. } => source,
            other => std::io::Error::other(format!("{other}")),
        }
    })?;
    let status = out
        .status
        .ok_or_else(|| std::io::Error::other("process aborted without a status"))?;
    let stdout = out.stdout_string();
    let stderr = out.stderr_string();
    Ok((status, stdout, stderr))
}

/// Like [`run_cmd`], but returns stdout as raw bytes.
///
/// Required for anything binary: the `String` variant goes through
/// `from_utf8_lossy`, which replaces every non-UTF-8 byte with U+FFFD. That
/// silently corrupts a binary payload — `adb exec-out screencap -p` returns a
/// PNG, so base64-encoding the lossy string yields something that is not a
/// decodable image at all.
pub async fn run_cmd_bytes(
    cmd: &str,
    args: &[&str],
) -> std::io::Result<(std::process::ExitStatus, Vec<u8>, String)> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(cmd.to_string());
    argv.extend(args.iter().map(|s| s.to_string()));

    let req = ProcessRequest::argv(&argv.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    let out = crate::tools::process_runner::run(&req)
        .await
        .map_err(|e| match e {
            ProcessError::Spawn { source, .. } => source,
            other => std::io::Error::other(format!("{other}")),
        })?;
    let status = out
        .status
        .ok_or_else(|| std::io::Error::other("process aborted without a status"))?;
    let stderr = out.stderr_string();
    Ok((status, out.stdout, stderr))
}

/// A screenshot tool's `region` argument, as a rectangle in screen points.
///
/// The computer adapters send the structured `{"x","y","width","height"}`
/// object, while every screenshot CLI these tools drive takes text in a
/// grammar of its own (`x,y,w,h`, `x,y WxH`, …). The *parsing* lives here
/// rather than in each platform so the shapes cannot drift: three adapters and
/// four tools all have to agree on what a region is, and a tool that silently
/// ignores one turns a close-up into a full-screen capture that the caller then
/// mistakes for a crop. Formatting for a particular CLI stays with that CLI,
/// because only it knows its separators.
///
/// `None` means "no usable region" — absent, malformed, or non-positive in
/// either dimension. Refusing is deliberate: a capture of a rectangle nobody
/// asked for is a wrong answer the caller cannot detect, while a missing crop
/// is at worst a wider view.
pub fn screenshot_region(args: &serde_json::Value) -> Option<(i64, i64, i64, i64)> {
    let region = args.get("region")?;
    if let Some(text) = region.as_str() {
        let parts: Vec<&str> = text.split([' ', ',', 'x']).map(str::trim).collect();
        let parts: Vec<&str> = parts.into_iter().filter(|p| !p.is_empty()).collect();
        if parts.len() != 4 {
            return None;
        }
        let values: Option<Vec<i64>> = parts.iter().map(|p| p.parse::<i64>().ok()).collect();
        return four(values?);
    }
    let x = region.get("x")?.as_i64()?;
    let y = region.get("y")?.as_i64()?;
    let width = region.get("width")?.as_i64()?;
    let height = region.get("height")?.as_i64()?;
    four(vec![x, y, width, height])
}

/// [`screenshot_region`], formatted as `x,y,w,h` — the form `maim -g`,
/// `import -crop` and `screencapture -R` all take.
pub fn screenshot_region_commas(args: &serde_json::Value) -> Option<String> {
    let (x, y, width, height) = screenshot_region(args)?;
    Some(format!("{x},{y},{width},{height}"))
}

/// [`screenshot_region`], formatted as `x,y WxH` — the form `grim -g` takes.
pub fn screenshot_region_grim(args: &serde_json::Value) -> Option<String> {
    let (x, y, width, height) = screenshot_region(args)?;
    Some(format!("{x},{y} {width}x{height}"))
}

/// Unpack four values and reject a rectangle with no area.
fn four(values: Vec<i64>) -> Option<(i64, i64, i64, i64)> {
    let [x, y, width, height]: [i64; 4] = values.try_into().ok()?;
    if width <= 0 || height <= 0 {
        return None;
    }
    Some((x, y, width, height))
}

/// Width and height from a PNG `IHDR` or a JPEG `SOFn` header.
///
/// The screenshot tools know what they captured but do not all say so: the
/// macOS tool reports `width`/`height`, the x11, Wayland and Windows tools emit
/// only the payload. Reading the size off the bytes gives every adapter one
/// answer that cannot disagree with the picture being sent.
///
/// `(0, 0)` for anything whose header is not recognised. That is the honest
/// result for a payload nobody parsed — inventing a plausible size would make
/// a broken capture look like a working one.
pub fn image_dimensions(data: &[u8]) -> (u32, u32) {
    // PNG: signature + IHDR chunk — width at offset 16, height at offset 20.
    if data.len() >= 24 && data[..8] == [137, 80, 78, 71, 13, 10, 26, 10] {
        let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        return (w, h);
    }
    // JPEG: find the SOF marker (0xFF 0xC0..0xC3 and 0xC5..0xC7 all carry it).
    if data.len() > 4 && data[0] == 0xFF && data[1] == 0xD8 {
        let mut i = 2;
        while i + 4 < data.len() {
            if data[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = data[i + 1];
            if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7) {
                // Segment: length (2), precision (1), height (2), width (2).
                if i + 9 < data.len() {
                    let h = u16::from_be_bytes([data[i + 5], data[i + 6]]);
                    let w = u16::from_be_bytes([data[i + 7], data[i + 8]]);
                    return (w as u32, h as u32);
                }
                return (0, 0);
            }
            // Standalone markers carry no length.
            if (0xD0..=0xD9).contains(&marker) || marker == 0x01 {
                i += 2;
                continue;
            }
            if i + 2 >= data.len() {
                break;
            }
            let seg_len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
            if seg_len < 2 {
                break;
            }
            i += 2 + seg_len;
        }
    }
    (0, 0)
}

/// Build a [`Screenshot`] from the `data` a screenshot tool returned.
///
/// This is the join that used to be written out by hand in every platform
/// adapter, and the reason it is one function now is that writing it three
/// times let it be wrong twice: the x11 and Windows adapters read a `base64`
/// key their tools have never emitted (they send `image_base64`), so every
/// capture on those platforms came back empty — no picture, no close-up, no
/// hover measurement — while reporting success. A hand-built struct also has to
/// remember `digest`, and two of the three literals had already been left
/// behind by the field's introduction.
///
/// Both key spellings are accepted because the desktop tools and the mobile
/// tools have always differed on this, and an adapter that guesses wrong about
/// which one it is talking to loses the whole frame.
pub fn screenshot_from_tool_data(data: Option<&serde_json::Value>) -> crate::computer::Screenshot {
    let base64 = data
        .and_then(|d| {
            d.get("image_base64")
                .or_else(|| d.get("base64"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or_default()
        .to_string();

    // Measure the bytes rather than trusting a reported size: a tool that
    // forgot to report one would otherwise hand the caller a 0×0 picture that
    // still has pixels, and every scale computation downstream divides by it.
    let (width, height) = crate::computer::types::base64_bytes(&base64)
        .map(|bytes| image_dimensions(&bytes))
        .filter(|(w, h)| *w != 0 && *h != 0)
        .unwrap_or_else(|| {
            let reported = |key: &str| {
                data.and_then(|d| d.get(key))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32
            };
            (reported("width"), reported("height"))
        });

    let mut screenshot = crate::computer::Screenshot::new(base64, width, height);
    screenshot.file_path = data
        .and_then(|d| d.get("file_path"))
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from);
    screenshot
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_adapters_object_becomes_the_cli_text_form() {
        let args = serde_json::json!({
            "region": { "x": 100, "y": 50, "width": 300, "height": 200 }
        });
        assert_eq!(screenshot_region(&args), Some((100, 50, 300, 200)));
        assert_eq!(screenshot_region_commas(&args).as_deref(), Some("100,50,300,200"));
    }

    #[test]
    fn a_region_string_is_understood_in_every_cli_grammar() {
        // `maim -g` / `import -crop` / `screencapture -R`
        let commas = serde_json::json!({ "region": " 10, 20 , 30,40 " });
        assert_eq!(screenshot_region(&commas), Some((10, 20, 30, 40)));
        // `grim -g` writes the origin and the size with different separators.
        let grim = serde_json::json!({ "region": "10,20 30x40" });
        assert_eq!(screenshot_region(&grim), Some((10, 20, 30, 40)));
        assert_eq!(
            screenshot_region_grim(&grim).as_deref(),
            Some("10,20 30x40"),
            "grim's own grammar, not a comma-joined one"
        );
    }

    #[test]
    fn no_region_is_not_a_refusal() {
        assert_eq!(screenshot_region(&serde_json::json!({})), None);
        assert_eq!(screenshot_region_commas(&serde_json::json!({})), None);
    }

    #[test]
    fn a_rectangle_with_no_area_is_refused() {
        for bad in [
            serde_json::json!({ "region": { "x": 0, "y": 0, "width": 0, "height": 10 } }),
            serde_json::json!({ "region": { "x": 0, "y": 0, "width": -5, "height": 10 } }),
            serde_json::json!({ "region": { "x": 0, "y": 0, "width": 10 } }),
            serde_json::json!({ "region": "0,0,ten,10" }),
            serde_json::json!({ "region": "0,0,0,10" }),
            serde_json::json!({ "region": "0,0,10" }),
        ] {
            assert_eq!(screenshot_region(&bad), None, "must refuse: {bad}");
        }
    }

    /// A minimal PNG header with the given size.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes
    }

    /// A minimal JPEG carrying a SOF0 with the given size.
    fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8];
        // An APP0-ish segment to be skipped over: length 4, two payload bytes.
        bytes.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]);
        // SOF0: length 11, precision 8, height, width, 1 component.
        bytes.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08]);
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
        bytes
    }

    /// The tool's `image_base64` becomes a picture the adapter can use.
    ///
    /// This is the defect this function exists to end: the x11 and Windows
    /// tools have always sent `image_base64`, their adapters read `base64`,
    /// and the result was an empty frame reported as a successful capture. The
    /// old code could not be tested from a machine that is neither platform;
    /// the join can.
    #[test]
    fn the_tools_payload_key_becomes_the_capture_payload() {
        use base64::Engine as _;
        let payload = base64::engine::general_purpose::STANDARD.encode(png(800, 600));
        let data = serde_json::json!({
            "image_base64": payload,
            "data_url": format!("data:image/png;base64,{payload}"),
            "format": "png",
            "size": 24,
        });

        let shot = screenshot_from_tool_data(Some(&data));
        assert_eq!(shot.base64, payload, "the payload must survive the join");
        assert_eq!((shot.width, shot.height), (800, 600), "and its size measured");
        assert!(
            shot.digest.is_some(),
            "a capture with bytes must carry the digest the store files it under"
        );
    }

    /// The size comes off the bytes, not out of an absent field.
    ///
    /// A tool that reports no `width` used to yield 0×0 even with a real
    /// picture inside — and every scale computation downstream divides by it.
    #[test]
    fn a_size_no_one_reported_is_read_off_the_picture() {
        use base64::Engine as _;
        let std_ = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);

        let png_shot = screenshot_from_tool_data(Some(&serde_json::json!({
            "image_base64": std_(&png(1280, 720)),
        })));
        assert_eq!((png_shot.width, png_shot.height), (1280, 720));

        let jpeg_shot = screenshot_from_tool_data(Some(&serde_json::json!({
            "image_base64": std_(&jpeg(1024, 768)),
        })));
        assert_eq!(
            (jpeg_shot.width, jpeg_shot.height),
            (1024, 768),
            "a JPEG payload must be measured too, not left 0×0"
        );
    }

    /// The other spelling still works, and a reported size is the fallback.
    #[test]
    fn both_spellings_are_understood() {
        let data = serde_json::json!({
            "base64": "AAAA",
            "width": 40,
            "height": 30,
        });
        let shot = screenshot_from_tool_data(Some(&data));
        assert_eq!(shot.base64, "AAAA");
        // Undecodable bytes cannot be measured, so the reported size is used.
        assert_eq!((shot.width, shot.height), (40, 30));
    }

    /// No payload is an empty capture, not a panic.
    #[test]
    fn a_tool_that_returned_no_data_is_an_empty_capture() {
        let shot = screenshot_from_tool_data(None);
        assert!(shot.base64.is_empty());
        assert_eq!((shot.width, shot.height), (0, 0));
        assert!(shot.digest.is_none(), "no bytes, nothing to name");
    }
}
