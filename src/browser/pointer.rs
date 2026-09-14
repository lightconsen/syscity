//! Pointer input as a sequence of CDP mouse events.
//!
//! A click and a drag are the same primitive: a button goes down, the pointer
//! moves, the button comes up. What differs is how many times, and whether the
//! moves matter. A drag that jumps from start to end in one step is not the
//! same gesture as one that passes through the points between — sliders, maps
//! and list-reordering code see movement, and only the second shape has any.

use chromiumoxide::cdp::browser_protocol::input::MouseButton;

/// A mouse button a caller can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Middle,
    Right,
}

impl PointerButton {
    /// Parse a caller's word for a button; absent or empty means left.
    pub fn parse(name: Option<&str>) -> Result<Self, String> {
        match name.map(str::trim).map(str::to_lowercase).as_deref() {
            None | Some("") | Some("left") => Ok(PointerButton::Left),
            Some("middle") => Ok(PointerButton::Middle),
            Some("right") => Ok(PointerButton::Right),
            Some(other) => {
                Err(format!("unknown mouse button `{other}` — use left, middle or right"))
            }
        }
    }

    /// The word this button is named by, for reporting back in a result.
    pub fn name(self) -> &'static str {
        match self {
            PointerButton::Left => "left",
            PointerButton::Middle => "middle",
            PointerButton::Right => "right",
        }
    }

    pub fn cdp(self) -> MouseButton {
        match self {
            PointerButton::Left => MouseButton::Left,
            PointerButton::Middle => MouseButton::Middle,
            PointerButton::Right => MouseButton::Right,
        }
    }

    /// The CDP `buttons` bitmask while this button is held: 1 left, 2 right,
    /// 4 middle.
    ///
    /// Not the same field as `button`. `button` says which one changed,
    /// `buttons` says which are held *afterwards* — and sending only `button`
    /// leaves a drag indistinguishable from a hover, which is exactly what the
    /// code that implements dragging checks.
    pub fn mask(self) -> i64 {
        match self {
            PointerButton::Left => 1,
            PointerButton::Middle => 4,
            PointerButton::Right => 2,
        }
    }
}

/// What a step does to the button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Move,
    Press,
    Release,
}

/// One mouse event in a sequence.
#[derive(Debug, Clone, PartialEq)]
pub struct PointerStep {
    pub kind: StepKind,
    pub x: f64,
    pub y: f64,
    /// Buttons held *after* this event.
    pub buttons: i64,
    pub click_count: i64,
}

/// More than a triple click is not a gesture any page distinguishes.
pub const MAX_CLICKS: u32 = 3;
/// Enough intermediate moves for a gesture to be tracked as movement, few
/// enough that a drag stays one round trip's worth of events.
pub const DEFAULT_DRAG_STEPS: u32 = 12;
pub const MAX_DRAG_STEPS: u32 = 60;

/// Clicks: `count` press/release pairs, numbered the way the browser expects.
///
/// A double-click is not one pair carrying `clickCount: 2`; the second press is
/// what carries the count, and the pair before it is what establishes that
/// there was a first click at all.
pub fn click_sequence(point: (f64, f64), button: PointerButton, count: u32) -> Vec<PointerStep> {
    let count = count.clamp(1, MAX_CLICKS);
    let mut steps = Vec::with_capacity(count as usize * 2);
    for n in 1..=count {
        steps.push(PointerStep {
            kind: StepKind::Press,
            x: point.0,
            y: point.1,
            buttons: button.mask(),
            click_count: i64::from(n),
        });
        steps.push(PointerStep {
            kind: StepKind::Release,
            x: point.0,
            y: point.1,
            buttons: 0,
            click_count: i64::from(n),
        });
    }
    steps
}

/// Drags: press at the start, move through the points between, release at the
/// end.
pub fn drag_sequence(
    from: (f64, f64),
    to: (f64, f64),
    steps: u32,
    button: PointerButton,
) -> Vec<PointerStep> {
    let steps = steps.clamp(1, MAX_DRAG_STEPS);
    let mut sequence = Vec::with_capacity(steps as usize + 3);

    // A move onto the start point first: the target needs the pointer there
    // before the press, and a press at a point the page never saw the pointer
    // arrive at is a different gesture on some toolkits.
    sequence.push(PointerStep {
        kind: StepKind::Move,
        x: from.0,
        y: from.1,
        buttons: 0,
        click_count: 0,
    });

    sequence.push(PointerStep {
        kind: StepKind::Press,
        x: from.0,
        y: from.1,
        buttons: button.mask(),
        click_count: 1,
    });

    for step in 1..=steps {
        let t = f64::from(step) / f64::from(steps);
        sequence.push(PointerStep {
            kind: StepKind::Move,
            x: from.0 + (to.0 - from.0) * t,
            y: from.1 + (to.1 - from.1) * t,
            buttons: button.mask(),
            click_count: 0,
        });
    }

    sequence.push(PointerStep {
        kind: StepKind::Release,
        x: to.0,
        y: to.1,
        buttons: 0,
        click_count: 1,
    });

    sequence
}

/// Check a caller's step count, defaulting and refusing rather than clamping
/// silently: a drag the caller asked to be finer than we will send should be
/// told, not quietly coarsened.
pub fn validated_steps(steps: Option<u32>) -> Result<u32, String> {
    match steps {
        None => Ok(DEFAULT_DRAG_STEPS),
        Some(0) => Err(
            "steps must be at least 1 — a drag with no movement between press and release is a \
             click"
                .to_string(),
        ),
        Some(n) if n > MAX_DRAG_STEPS => Err(format!(
            "steps {n} is more than the {MAX_DRAG_STEPS} intermediate moves this will send"
        )),
        Some(n) => Ok(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_button_word_maps_to_the_right_mask() {
        assert_eq!(PointerButton::parse(None).unwrap(), PointerButton::Left);
        assert_eq!(PointerButton::parse(Some("")).unwrap(), PointerButton::Left);
        assert_eq!(PointerButton::parse(Some(" Right ")).unwrap(), PointerButton::Right);
        assert_eq!(PointerButton::parse(Some("MIDDLE")).unwrap(), PointerButton::Middle);
    }

    #[test]
    fn a_button_word_that_is_not_one_is_refused() {
        let refused = PointerButton::parse(Some("primary")).unwrap_err();
        assert!(refused.contains("primary"), "{refused}");
        assert!(refused.contains("left, middle or right"), "{refused}");
    }

    #[test]
    fn the_button_mask_follows_the_cdp_convention() {
        // 1 left, 2 right, 4 middle — the order is the spec's, not ours.
        assert_eq!(PointerButton::Left.mask(), 1);
        assert_eq!(PointerButton::Right.mask(), 2);
        assert_eq!(PointerButton::Middle.mask(), 4);
    }

    #[test]
    fn a_single_click_is_one_pair() {
        let steps = click_sequence((10.0, 20.0), PointerButton::Left, 1);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].kind, StepKind::Press);
        assert_eq!(steps[1].kind, StepKind::Release);
        assert_eq!(steps[0].buttons, 1, "held after the press");
        assert_eq!(steps[1].buttons, 0, "nothing held after the release");
        assert!(steps.iter().all(|s| (s.x, s.y) == (10.0, 20.0)));
    }

    #[test]
    fn a_double_click_is_two_pairs_with_the_count_on_the_second() {
        let steps = click_sequence((1.0, 2.0), PointerButton::Right, 2);
        let kinds: Vec<_> = steps.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                StepKind::Press,
                StepKind::Release,
                StepKind::Press,
                StepKind::Release
            ]
        );
        let counts: Vec<_> = steps.iter().map(|s| s.click_count).collect();
        assert_eq!(counts, vec![1, 1, 2, 2]);
        assert_eq!(steps[2].buttons, 2, "the right button is held");
    }

    #[test]
    fn an_absurd_click_count_is_clamped_not_sent() {
        assert_eq!(click_sequence((0.0, 0.0), PointerButton::Left, 99).len(), 6);
        // Zero means "at least a click"; there is no such thing as fewer.
        assert_eq!(click_sequence((0.0, 0.0), PointerButton::Left, 0).len(), 2);
    }

    #[test]
    fn a_drag_puts_the_button_down_at_the_start_and_up_at_the_end() {
        let steps = drag_sequence((0.0, 0.0), (120.0, 40.0), 4, PointerButton::Left);
        assert_eq!(steps.first().unwrap().kind, StepKind::Move);
        let press = steps.iter().find(|s| s.kind == StepKind::Press).unwrap();
        assert_eq!((press.x, press.y), (0.0, 0.0));
        assert_eq!(press.buttons, 1);

        let release = steps.last().unwrap();
        assert_eq!(release.kind, StepKind::Release);
        assert_eq!((release.x, release.y), (120.0, 40.0));
        assert_eq!(release.buttons, 0, "nothing held once it has been released");
    }

    #[test]
    fn a_drag_passes_through_the_points_between() {
        let steps = drag_sequence((0.0, 0.0), (100.0, 0.0), 4, PointerButton::Left);
        let moves: Vec<(f64, f64)> = steps
            .iter()
            .filter(|s| s.kind == StepKind::Move)
            .map(|s| (s.x, s.y))
            .collect();
        // The first move is onto the start point, then four along the way.
        assert_eq!(
            moves,
            vec![
                (0.0, 0.0),
                (25.0, 0.0),
                (50.0, 0.0),
                (75.0, 0.0),
                (100.0, 0.0)
            ]
        );
    }

    #[test]
    fn every_move_between_press_and_release_holds_the_button_down() {
        // This is the field that separates a drag from a hover; getting it
        // wrong leaves pages that check `event.buttons` doing nothing.
        let steps = drag_sequence((5.0, 5.0), (50.0, 60.0), 3, PointerButton::Middle);
        let press = steps
            .iter()
            .position(|s| s.kind == StepKind::Press)
            .unwrap();
        let release = steps
            .iter()
            .position(|s| s.kind == StepKind::Release)
            .unwrap();
        for step in &steps[press..release] {
            assert_eq!(step.buttons, 4, "{step:?} should hold the middle button");
        }
    }

    #[test]
    fn a_drag_to_the_same_point_is_still_a_drag() {
        let steps = drag_sequence((7.0, 7.0), (7.0, 7.0), 2, PointerButton::Left);
        assert_eq!(steps.last().unwrap().kind, StepKind::Release);
        assert!(steps.iter().all(|s| (s.x, s.y) == (7.0, 7.0)));
    }

    #[test]
    fn a_step_count_that_cannot_be_honoured_is_refused() {
        assert_eq!(validated_steps(None).unwrap(), DEFAULT_DRAG_STEPS);
        assert_eq!(validated_steps(Some(5)).unwrap(), 5);

        let zero = validated_steps(Some(0)).unwrap_err();
        assert!(zero.contains("is a click"), "{zero}");

        let too_many = validated_steps(Some(MAX_DRAG_STEPS + 1)).unwrap_err();
        assert!(too_many.contains(&MAX_DRAG_STEPS.to_string()), "{too_many}");
    }
}
