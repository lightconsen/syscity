//! Scrolling a screen a page at a time, and aiming at its container.

use super::*;

/// Which way a scroll moves the content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Down,
    Up,
}

impl Direction {
    pub(super) fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("down").to_ascii_lowercase().as_str() {
            "down" | "forward" => Ok(Self::Down),
            "up" | "back" | "backward" => Ok(Self::Up),
            other => Err(format!("direction must be 'down' or 'up', not {other:?}")),
        }
    }
}

/// The device's screen size, from the dump or from `wm size`.
pub(crate) async fn screen_size(
    device: &Option<String>,
    from_dump: Option<(i32, i32)>,
) -> Option<(i32, i32)> {
    if let Some(size) = from_dump {
        return Some(size);
    }
    let args = adb_args(device, &["shell", "wm", "size"]);
    let (status, stdout, _) = run_cmd("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .await
        .ok()?;
    if !status.success() {
        return None;
    }
    // `Physical size: 1080x2340` (an `Override size:` line may follow).
    stdout.lines().find_map(|line| {
        let value = line.split(':').nth(1)?.trim();
        let (width, height) = value.split_once('x')?;
        Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
    })
}

/// Scroll one screenful, aiming at the scrollable container when the tree named
/// one and falling back to a swipe down the middle of the display.
///
/// The container wins when there is one: a swipe starts at a point, and on a
/// screen with a nested list, a bottom bar or a sideways carousel that point
/// may land on something that also consumes drags — the scroll then moves the
/// wrong thing, or nothing. A container is addressed as a whole, so where
/// inside it the gesture starts stops mattering. When the tree names none (the
/// list is custom-drawn, or the tree could not be read) the swipe is still the
/// only option, and it is what every scroll did before this.
pub(crate) async fn scroll_once(
    device: &Option<String>,
    size: (i32, i32),
    direction: Direction,
    target: Target,
    container: Option<&UiElement>,
) -> Result<(), String> {
    let (from, to) = scroll_points(size, direction, container);
    if let Target::Display(id) = target {
        // A swipe aimed at the agent's display goes through the helper: ADB's
        // own `input` only reaches the default display.
        let bridge = display_bridge()?;
        let display = super::display::VirtualDisplay::attach(bridge.clone(), id);
        return display
            .swipe(from, to, 300)
            .await
            .map_err(|e| e.to_string());
    }
    let command = format!("input swipe {} {} {} {} 300", from.0, from.1, to.0, to.1);
    let args = adb_args(device, &["shell", &command]);
    let (status, _, stderr) = run_cmd("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .await
        .map_err(|e| format!("could not run adb to scroll: {e}"))?;
    if !status.success() {
        return Err(format!("scrolling failed: {}", stderr.trim()));
    }
    Ok(())
}

/// Where one screenful's drag starts and ends.
///
/// A container gets the gesture *inside itself*, split across its height, so the
/// drag stays within the thing that is supposed to move. Shared with
/// `android_run`, which scrolls through `android_input` so the step's guard and
/// effect check still apply to it.
pub(crate) fn scroll_points(
    size: (i32, i32),
    direction: Direction,
    container: Option<&UiElement>,
) -> ((i32, i32), (i32, i32)) {
    let (width, height) = size;
    let full_screen = || {
        let (from, to) = match direction {
            Direction::Down => (height * 3 / 4, height / 4),
            Direction::Up => (height / 4, height * 3 / 4),
        };
        ((width / 2, from), (width / 2, to))
    };
    match container {
        Some(node) => {
            let (x1, y1, x2, y2) = node.bounds;
            let span = y2 - y1;
            // A node too short to hold a real drag (a two-row list, a sliver)
            // cannot express a screenful; the whole screen still can.
            if span < 200 || x2 <= x1 {
                return full_screen();
            }
            let (from, to) = match direction {
                Direction::Down => (y1 + span * 3 / 4, y1 + span / 4),
                Direction::Up => (y1 + span / 4, y1 + span * 3 / 4),
            };
            ((node.center.0, from), (node.center.0, to))
        }
        None => full_screen(),
    }
}

/// The scrollable container a scroll should be aimed at, if the tree names one.
///
/// Innermost first (deepest wins) among those that cover the middle of the
/// screen: the outer container of a chat screen covers everything, including
/// the input bar, while the message list is the inner one — and it is the
/// message list that a "scroll down" is about. Ties go to the larger node,
/// which is the one holding more content.
pub(crate) fn scroll_container(
    elements: &[UiElement],
    size: (i32, i32),
    direction: Direction,
) -> Option<&UiElement> {
    let (_, height) = size;
    let midline = height / 2;
    elements
        .iter()
        .filter(|element| {
            element.state.scrollable && element.bounds.1 <= midline && element.bounds.3 >= midline
        })
        .filter(|element| match direction {
            Direction::Down => element.state.scroll_forward,
            Direction::Up => element.state.scroll_backward,
        })
        .max_by_key(|element| {
            let (x1, y1, x2, y2) = element.bounds;
            (element.state.depth, (x2 - x1) as i64 * (y2 - y1) as i64)
        })
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;

    #[test]
    fn directions_parse_both_spellings() {
        assert_eq!(Direction::parse(Some("down")).expect("down"), Direction::Down);
        assert_eq!(Direction::parse(Some("UP")).expect("up"), Direction::Up);
        assert_eq!(Direction::parse(None).expect("default"), Direction::Down);
        assert!(Direction::parse(Some("sideways")).is_err());
    }
    #[test]
    fn a_scroll_prefers_the_innermost_container_covering_the_middle() {
        // An outer container covering the whole screen and the message list
        // inside it. A "scroll down" is about the list, not about everything.
        let outer = scrollable(1, (0, 0, 1080, 2340));
        let list = scrollable(3, (0, 300, 1080, 2000));
        let elements = vec![outer, list];
        let picked =
            scroll_container(&elements, (1080, 2340), Direction::Down).expect("a container");
        assert_eq!(picked.state.depth, 3);
    }
    #[test]
    fn a_container_that_does_not_cover_the_middle_is_not_picked() {
        // A sideways carousel near the top: it scrolls, but not the page.
        let carousel = scrollable(2, (0, 0, 1080, 400));
        let elements = vec![carousel];
        assert!(scroll_container(&elements, (1080, 2340), Direction::Down).is_none());
    }
    #[test]
    fn a_one_direction_container_only_takes_that_direction() {
        let mut up_only = scrollable(2, (0, 100, 1080, 2300));
        up_only.state.scroll_forward = false;
        let elements = vec![up_only];
        assert!(scroll_container(&elements, (1080, 2340), Direction::Down).is_none());
        assert!(scroll_container(&elements, (1080, 2340), Direction::Up).is_some());
    }
    #[test]
    fn a_tree_without_a_container_leaves_the_swipe_in_charge() {
        let mut label = UiElement::new(
            1,
            Some("Hello".to_string()),
            None,
            None,
            "TextView".to_string(),
            false,
            true,
            (0, 0, 200, 60),
        );
        label.state.depth = 2;
        assert!(scroll_container(&[label], (1080, 2340), Direction::Down).is_none());
    }
}
