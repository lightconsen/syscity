//! The AT-SPI2 read, shared by the X11 and Wayland tools.
//!
//! AT-SPI2 is a D-Bus accessibility *bus*, not an X11 protocol. GNOME and KDE
//! expose it under Wayland exactly as they do under X11, so the tree a Wayland
//! session can be asked for is the same tree an X11 session can — the only
//! X11-specific part of the older reader was its `xdotool` fallback, which
//! reads window geometry and knows nothing about the tree.
//!
//! Both tools therefore run this one script. Keeping one copy is the point:
//! the Wayland tool used to have *no* read at all (it looked for an
//! `xdg-desktop-portal` accessibility API that does not exist) and reported
//! that as a successful empty tree, so a Wayland session looked like a desktop
//! with no controls on it instead of a desktop this program could not read.
//!
//! # What the script returns
//!
//! A JSON object for the active application, nested `children` and all. Two
//! fields exist purely so the caller can tell a *whole* tree from a *cut* one:
//! `children_count` (what the platform said the node has) and `truncated` (the
//! depth limit stopped the walk with children unread). A platform that reports
//! neither has not said its tree is complete — it has said nothing.

use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

/// How long one AT-SPI2 read may take.
///
/// A disabled accessibility bridge can hold the D-Bus call for tens of
/// seconds; the tool has a caller waiting, so it states its own bound rather
/// than inheriting the caller's.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// What one AT-SPI2 read produced.
#[derive(Debug, Clone)]
pub struct AtspiRead {
    /// The script exited, printed JSON, and that JSON parsed.
    pub ok: bool,
    /// The parsed tree for the active application, when `ok`.
    pub tree: Option<serde_json::Value>,
    /// The app name the tree belongs to, when the script named one.
    pub app: Option<String>,
    /// What went wrong, when `ok` is false. Never empty.
    pub error: Option<String>,
    /// The raw stdout, for callers that want to show what the reader saw.
    pub raw: Option<String>,
}

impl AtspiRead {
    /// A read that could not produce a tree.
    ///
    /// `error` is always populated: a failure with no reason is the failure
    /// this type exists to prevent.
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            tree: None,
            app: None,
            error: Some(error.into()),
            raw: None,
        }
    }
}

/// The Python source that walks the active application.
///
/// `max_depth` is substituted rather than frozen: a caller asking for a deeper
/// tree should not be silently given a shallow one, which reads as "the
/// element is not there".
///
/// The script is a template rather than a `format!` string because its Python
/// is full of `{}` dict literals, which would have to be doubled to survive
/// formatting.
pub fn python_script(max_depth: u32) -> String {
    script_for(
        max_depth,
        r#"
active_app = None
for i in range(desktop.childCount):
    app = desktop.getChildAtIndex(i)
    try:
        if app.getState().contains(pyatspi.STATE_ACTIVE) or app.getState().contains(pyatspi.STATE_FOCUSED):
            active_app = app
            break
    except Exception:
        pass

if not active_app and desktop.childCount > 0:
    active_app = desktop.getChildAtIndex(0)

if active_app:
    tree = export_element(active_app)
    print(json.dumps(tree))
else:
    print(json.dumps({"role": "desktop", "name": "", "children": []}))
"#,
    )
}

/// The Python source that walks **every** application on the desktop.
///
/// The active-application read ([`python_script`]) is right for a local
/// session, where the caller is looking at one window. It is the wrong shape
/// for a remote desktop driven over SSH: nothing on that side has focus
/// because the SSH session is not a desktop session, so "the active app" is
/// whatever the accessibility bus happens to report — and on XFCE that can be
/// the session manager, whose tree is empty while the terminal the goal needs
/// is sitting right there.
///
/// The roots are one `desktop` node holding every application, which is what
/// lets element ids name a control in *any* window and lets the caller prune
/// the off-screen ones by geometry.
pub fn desktop_python_script(max_depth: u32) -> String {
    script_for(
        max_depth,
        r#"
apps = []
for i in range(desktop.childCount):
    try:
        app = desktop.getChildAtIndex(i)
    except Exception:
        continue
    tree = export_element(app)
    if tree:
        apps.append(tree)
print(json.dumps({
    "role": "desktop",
    "name": "",
    "children_count": len(apps),
    "truncated": False,
    "children": apps,
}))
"#,
    )
}

/// Wrap [`export_element`](python_script) and the walk's last line in one
/// script.
///
/// The Python body is shared so the two readers cannot disagree about how an
/// element is shaped — a second copy would drift, and the disagreement would
/// show up as a tree one caller calls complete and the other calls truncated.
fn script_for(max_depth: u32, roots: &str) -> String {
    let body = r#"
import json, sys
try:
    import pyatspi
except ImportError:
    print(json.dumps({"error": "pyatspi not installed"}), file=sys.stderr)
    sys.exit(1)

def geometry(acc):
    # `Accessible.extents` is not an attribute on every version of pyatspi
    # (application nodes on XFCE raise AttributeError), and treating that as
    # "this element is unreadable" dropped whole applications from the tree.
    # Geometry is optional; the element is not.
    try:
        ext = acc.extents
        return int(ext.x), int(ext.y), int(ext.width), int(ext.height)
    except Exception:
        pass
    try:
        ext = acc.queryComponent().getExtents(0)
        return int(ext.x), int(ext.y), int(ext.width), int(ext.height)
    except Exception:
        return None

def export_element(acc, depth=0):
    try:
        name = acc.name or ""
        role = acc.getRoleName() or "unknown"
        state_set = acc.getState()
        enabled = state_set.contains(pyatspi.STATE_ENABLED)
        focused = state_set.contains(pyatspi.STATE_FOCUSED)
    except Exception:
        return None
    try:
        child_count = int(acc.childCount)
    except Exception:
        child_count = None
    obj = {
        "role": role,
        "name": name,
        "enabled": enabled,
        "focused": focused,
        "children_count": child_count,
        "truncated": False,
        "children": []
    }
    # Only publish a box when there is one. A made-up (0,0,0,0) is a point in
    # the screen's corner that no control occupies, and a model that clicks it
    # would be aimed at nothing.
    box = geometry(acc)
    if box is not None:
        obj["x"], obj["y"], obj["width"], obj["height"] = box
    if depth < __MAX_DEPTH__:
        if child_count:
            for i in range(child_count):
                try:
                    child = acc.getChildAtIndex(i)
                except Exception:
                    continue
                child_dict = export_element(child, depth + 1)
                if child_dict:
                    obj["children"].append(child_dict)
    else:
        # Stopped at the depth limit with children still unread. The count is
        # what makes that visible: the caller can tell "no children" from
        # "children nobody looked at".
        obj["truncated"] = bool(child_count)
    return obj

desktop = pyatspi.Registry.getDesktop(0)
__ROOTS__
"#;
    body.replace("__ROOTS__", roots)
        .replace("__MAX_DEPTH__", &max_depth.to_string())
}

/// Read the active application's tree over the AT-SPI2 bus.
///
/// Never panics and never hangs past [`READ_TIMEOUT`]: a read that fails is a
/// [`AtspiRead`] with `ok == false` and a reason, which is what lets the
/// callers say "this screen could not be read" instead of handing back an
/// empty tree that looks like a desktop with nothing on it.
pub async fn read(max_depth: u32) -> AtspiRead {
    let script = python_script(max_depth);
    let output =
        timeout(READ_TIMEOUT, Command::new("python3").arg("-c").arg(&script).output()).await;

    let out = match output {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return AtspiRead::failed(format!("python3 could not be started: {e}")),
        Err(_) => {
            return AtspiRead::failed(format!(
                "the accessibility read did not answer within {}s — is the AT-SPI bridge \
                 (python3-pyatspi) installed?",
                READ_TIMEOUT.as_secs()
            ))
        }
    };

    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();

    if !out.status.success() {
        // pyatspi missing is the common case and it prints a JSON reason to
        // stderr; anything else is passed through as it came.
        let reason = if stderr.is_empty() {
            format!("python3 exited with {}", out.status)
        } else {
            stderr
        };
        return AtspiRead::failed(reason);
    }
    if stdout.is_empty() {
        return AtspiRead::failed("the accessibility read printed nothing");
    }

    match serde_json::from_str::<serde_json::Value>(&stdout) {
        Ok(tree) => {
            let app = tree
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            AtspiRead {
                ok: true,
                tree: Some(tree),
                app,
                error: None,
                raw: Some(stdout),
            }
        }
        Err(e) => AtspiRead::failed(format!("the accessibility read did not return JSON: {e}")),
    }
}

/// The X11-only fallback: window geometry from `xdotool` + `xwininfo`.
///
/// This is the one part of the X11 reader with no Wayland equivalent, and it
/// is a fallback for a reason — it yields the focused window's name and box,
/// not its controls.
pub fn xwininfo_fallback_script() -> &'static str {
    r#"
import json, subprocess, sys

def run(cmd):
    try:
        out = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=5)
        return out.stdout.strip()
    except Exception:
        return ""

# Get active window id
wid = run("xdotool getactivewindow 2>/dev/null || xdotool getwindowfocus 2>/dev/null")
if not wid:
    print(json.dumps({"role":"desktop","name":"","children":[]}))
    sys.exit(0)

# Get window info
info = run(f"xwininfo -id {wid} 2>/dev/null")
name = ""
for line in info.splitlines():
    if "Window id:" in line and '"' in line:
        name = line.split('"')[1]
        break

# Get window geometry
geo = run(f"xdotool getwindowgeometry {wid} 2>/dev/null")
x, y, w, h = 0, 0, 0, 0
for line in geo.splitlines():
    if "Position:" in line:
        parts = line.replace(",", "").split()
        try:
            x = int(parts[1])
            y = int(parts[2])
        except Exception:
            pass
    if "Geometry:" in line:
        parts = line.replace("x", " ").split()
        try:
            w = int(parts[1])
            h = int(parts[2])
        except Exception:
            pass

print(json.dumps({
    "role": "Window",
    "name": name,
    "enabled": True,
    "x": x, "y": y,
    "width": w, "height": h,
    "children": []
}))
"#
}

/// Run an arbitrary Python helper (the X11 fallback uses this).
pub async fn run_python(script: &str) -> crate::Result<(bool, String, String)> {
    let output =
        timeout(READ_TIMEOUT, Command::new("python3").arg("-c").arg(script).output()).await;
    match output {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Ok((out.status.success() && !stdout.is_empty(), stdout, stderr))
        }
        Ok(Err(e)) => Ok((false, String::new(), format!("Python spawn error: {e}"))),
        Err(_) => Ok((false, String::new(), "Python timed out".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_honours_the_requested_depth() {
        let script = python_script(7);
        assert!(script.contains("if depth < 7:"), "{script}");
        assert!(!script.contains("__MAX_DEPTH__"), "placeholder left in script");
        // The dict literals survive templating untouched.
        assert!(script.contains(r#"{"error": "pyatspi not installed"}"#));
        // Focus decides where a bare `type` / `key_press` lands.
        assert!(script.contains("STATE_FOCUSED"), "{script}");
        assert!(script.contains(r#""focused": focused,"#), "{script}");
    }

    /// A depth of zero is a legitimate request (the app node only), and it
    /// must not be rewritten into something that walks the whole tree.
    #[test]
    fn a_shallow_read_asks_for_exactly_that() {
        let script = python_script(0);
        assert!(script.contains("if depth < 0:"), "{script}");
    }

    /// The completeness fields are the difference between "the desktop has no
    /// controls" and "the walk stopped"; the script must keep emitting both.
    #[test]
    fn the_script_reports_enough_to_judge_completeness() {
        let script = python_script(3);
        assert!(script.contains(r#""children_count": child_count,"#), "{script}");
        assert!(script.contains(r#"obj["truncated"] = bool(child_count)"#), "{script}");
    }

    #[test]
    fn a_failure_always_carries_a_reason() {
        let read = AtspiRead::failed("pyatspi not installed");
        assert!(!read.ok);
        assert_eq!(read.error.as_deref(), Some("pyatspi not installed"));
        assert!(read.tree.is_none());
    }

    /// An application node on XFCE has no `.extents`, and the reader used to
    /// treat that as an unreadable element — which dropped whole applications
    /// from the tree and left the remote adapter with nothing to aim at.
    #[test]
    fn geometry_is_optional_but_the_element_is_not() {
        let script = python_script(4);
        assert!(script.contains("def geometry(acc):"), "{script}");
        // The fallback is a real second attempt, not a default of (0,0,0,0).
        assert!(script.contains("acc.queryComponent().getExtents(0)"), "{script}");
        // A missing box omits the keys rather than claiming the screen corner.
        assert!(script.contains("if box is not None:"), "{script}");
    }

    /// The desktop read must name *every* application: that is what makes an
    /// element id resolvable in a window that is not the focused one, which an
    /// SSH-driven session always has.
    #[test]
    fn the_desktop_read_walks_every_application() {
        let script = desktop_python_script(6);
        assert!(script.contains("apps.append(tree)"), "{script}");
        assert!(script.contains(r#""children": apps,"#), "{script}");
        assert!(script.contains("if depth < 6:"), "{script}");
        // It shares the element shape with the active-app read.
        assert!(script.contains("def export_element(acc, depth=0):"), "{script}");
        assert!(script.contains(r#""children_count": child_count,"#), "{script}");
    }

    /// A `None` child (pyatspi hands one back when a node goes away mid-walk)
    /// must not take the whole run down with an AttributeError.
    #[test]
    fn a_vanished_child_is_skipped_not_fatal() {
        let script = python_script(3);
        assert!(script.contains("except Exception:\n                    continue"), "{script}");
    }

    /// The fallback is X11-specific and must say so, not silently look like
    /// the shared reader: it drives `xdotool`.
    #[test]
    fn the_x11_fallback_is_actually_x11() {
        let script = xwininfo_fallback_script();
        assert!(script.contains("xdotool"), "{script}");
        assert!(script.contains(r#""role": "Window""#), "{script}");
    }
}
