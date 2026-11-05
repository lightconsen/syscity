//! Wheel pickers — the time/date/region controls that turn one item at a time.
//!
//! A wheel is the one control a model cannot drive well by looking: every turn
//! renumbers the screen, the selected item is usually only distinguishable by
//! being in the middle, and the number of turns to a value depends on how far
//! away it is (and, on a looping wheel, which way round is shorter). Doing it
//! a tap at a time costs a turn per item; doing it here costs one call.
//!
//! Everything in this module is arithmetic and recognition — no device, no I/O,
//! no async. [`WheelSteps`] and [`WheelDrags`] are planners: they are told what
//! the wheel reads now and answer what to do next. The tool does the reading,
//! the scrolling and the dragging, which is the only part that needs a phone.
//!
//! Recognition is deliberately conservative ([`looks_like_wheel`]): a chat list
//! and a settings page also have evenly spaced short labels, so the check asks
//! for the things *only* a wheel has — a narrow run of items, all centred on the
//! same vertical line, with even gaps — and misses a wheel rather than calling
//! a list one.

use std::sync::OnceLock;

use regex::Regex;

/// A value as it is written on the wheel: `9`, `09`, `2026`, `PM`, `Beijing`.
///
/// Kept as a scan rather than a pattern because the `regex` crate refuses
/// look-around, and the look-behind is what makes "a standalone number" mean
/// one: `12345678` is a timestamp, not a value, and neither end may be a digit
/// that was cut off by the six-digit form.
fn standalone_numbers(text: &str) -> Vec<&str> {
    let mut runs = Vec::new();
    let bytes = text.as_bytes();
    let mut start = None;
    for (index, byte) in bytes.iter().enumerate() {
        if byte.is_ascii_digit() {
            start.get_or_insert(index);
        } else if let Some(from) = start.take() {
            runs.push((from, index));
        }
    }
    if let Some(from) = start {
        runs.push((from, text.len()));
    }
    runs.into_iter()
        .filter(|(from, to)| to - from <= 6)
        .map(|(from, to)| &text[from..to])
        .collect()
}

/// Punctuation separating a value from a status word (`已选中，21`) or a unit.
fn separators() -> &'static Regex {
    // The one static a pattern earns: this is a constant pattern, checked by
    // a test, and a compile failure here cannot be handled at any call site.
    static RE: OnceLock<Regex> = OnceLock::new();
    #[allow(clippy::unwrap_used)]
    RE.get_or_init(|| Regex::new(r"[,，、:：;；|/()（）\[\]【】<>《》·•]+").unwrap())
}

/// The comparison form of a label: trimmed, whitespace removed, full-width
/// digits folded to ASCII, lower-cased.
///
/// The accessibility name of a wheel item routinely arrives with the state
/// glued on (`已选中，21`, `Selected, 21`), and a Chinese keyboard produces
/// full-width digits (`２０２６`). Both have to compare equal to the value a
/// caller typed.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.trim().chars() {
        if c.is_whitespace() {
            continue;
        }
        match c {
            '０'..='９' => out.push(char::from(b'0' + (c as u32 - '０' as u32) as u8)),
            _ => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// The integer a wheel item displays.
///
/// `"09"` is 9, `"9时"` is 9, `"2026年"` is 2026, `"已选中，21"` is 21. When the
/// label holds exactly one number that number is the value wherever it sits;
/// with several, only one within the first few characters can be it.
pub fn number(label: Option<&str>) -> Option<i32> {
    let normalized = normalize(label?);
    if normalized.is_empty() {
        return None;
    }
    let (negative, body) = match normalized.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, normalized.as_str()),
    };
    if !body.is_empty() && body.len() <= 6 && body.bytes().all(|b| b.is_ascii_digit()) {
        return normalized.parse().ok();
    }
    if negative {
        // A minus that is not part of a leading number is a separator or a
        // dash, and the digits after it are not the whole value.
        return None;
    }
    let all = standalone_numbers(&normalized);
    match all.as_slice() {
        // Exactly one number: it is what the label shows, wherever it sits.
        [single] => single.parse().ok(),
        // Several: only one just after the start can be the value; the rest of
        // the label is then prose ("现在是北京时间 21 和 22").
        _ => leading_number(&normalized).and_then(|found| found.parse().ok()),
    }
}

/// The first number that starts within the first few characters, if any.
///
/// "Near the start" is measured in `char`s, not bytes, because the characters
/// in front of a value are usually CJK.
fn leading_number(normalized: &str) -> Option<&str> {
    let mut seen = 0_usize;
    for (at, c) in normalized.char_indices() {
        if c.is_ascii_digit() {
            // A digit that belongs to a longer run is not a standalone number.
            let end = normalized[at..]
                .find(|c: char| !c.is_ascii_digit())
                .map_or(normalized.len(), |offset| at + offset);
            if end - at <= 6 && seen <= 3 {
                return Some(&normalized[at..end]);
            }
            return None;
        }
        seen += 1;
        if seen > 4 {
            return None;
        }
    }
    None
}

/// The label split at its separators, for input that has already been
/// normalised: `已选中，21` → `[已选中, 21]`.
pub fn segments_of(normalized: &str) -> Vec<&str> {
    separators()
        .split(normalized)
        .filter(|part| !part.is_empty())
        .collect()
}

/// The value to report to the model: a label that carries a status word is
/// reduced to the part that holds the value (`已选中，21` → `21`).
///
/// Without a unique part that contains a number the label is reported as it
/// was: a text wheel (`Beijing`) has no status word to strip, and guessing
/// which half is the value would be worse than quoting the whole thing.
pub fn clean(label: Option<&str>) -> Option<String> {
    let trimmed = label?.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parts: Vec<&str> = separators()
        .split(trimmed)
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() < 2 {
        return Some(trimmed.to_string());
    }
    let numeric: Vec<&str> = parts
        .iter()
        .copied()
        .filter(|part| {
            part.chars()
                .any(|c| c.is_ascii_digit() || ('０'..='９').contains(&c))
        })
        .collect();
    match numeric.as_slice() {
        [single] => Some((*single).to_string()),
        _ => Some(trimmed.to_string()),
    }
}

/// The target as a number, when it is one (`9`, `09`, `２０２６`).
///
/// A target that merely *contains* a number (`40分`) is not numeric: the wheel
/// shows a value, and `40分` could be either that value with its unit or a
/// label of its own.
pub fn numeric_target(target: &str) -> Option<i32> {
    let normalized = normalize(target);
    match normalized.as_str() {
        "" => None,
        text => {
            let digits = text.strip_prefix('-').unwrap_or(text);
            (!digits.is_empty() && digits.len() <= 6 && digits.bytes().all(|b| b.is_ascii_digit()))
                .then(|| text.parse().ok())
                .flatten()
        }
    }
}

/// Whether the wheel showing `shown` is showing `target`.
///
/// A numeric target compares numerically, which is what makes `09` equal `9`
/// and `已选中，21` equal `21`. A text target compares literally or
/// as an affix of one of the label's parts, so `北京` finds `北京市`.
pub fn matches(shown: Option<&str>, target: &str) -> bool {
    let Some(shown) = shown.map(normalize).filter(|s| !s.is_empty()) else {
        return false;
    };
    let target = normalize(target);
    if target.is_empty() {
        return false;
    }
    if let Some(wanted) = numeric_target(target.as_str()) {
        return number(Some(&shown)) == Some(wanted);
    }
    let affix = |part: &str| part == target || part.starts_with(&target) || part.ends_with(&target);
    affix(&shown) || segments_of(&shown).into_iter().any(affix)
}

/// The length of a looping wheel, from the items visible right now.
///
/// A clock's minute column reads `…58 59 00 01…`: exactly one place where the
/// number goes down, and the loop is everything from the largest value before
/// that break to the smallest after it. Two breaks, no break, or non-numeric
/// items mean the length is not visible, and the caller must walk without it.
pub fn modulus(visible: &[String]) -> Option<i32> {
    let mut numbers = Vec::with_capacity(visible.len());
    for item in visible {
        numbers.push(number(Some(item))?);
    }
    if numbers.len() < 2 {
        return None;
    }
    let drops: Vec<usize> = (1..numbers.len())
        .filter(|&i| numbers[i] < numbers[i - 1])
        .collect();
    let [at] = drops.as_slice() else {
        return None;
    };
    let highest = numbers[..*at].iter().copied().max()?;
    let lowest = numbers[*at..].iter().copied().min()?;
    let span = highest - lowest + 1;
    (span >= 2).then_some(span)
}

/// How many items towards larger values [`current`] is from [`target`];
/// negative when the short way round is downwards.
///
/// With a known loop the answer is the shorter of the two ways round, which is
/// what turns `59 → 0` into one step rather than fifty-nine.
pub fn distance(current: i32, target: i32, modulus: Option<i32>) -> i32 {
    let direct = target - current;
    let Some(modulus) = modulus.filter(|m| *m >= 2) else {
        return direct;
    };
    let stepped = ((direct % modulus) + modulus) % modulus;
    if stepped <= modulus / 2 {
        stepped
    } else {
        stepped - modulus
    }
}

/// How many items the reading moved between `before` and `after`.
///
/// `None` when either reading is not a number — a text wheel has no distance,
/// and calling it zero would make a list look stuck.
pub fn moved(before: Option<&str>, after: Option<&str>, modulus: Option<i32>) -> Option<i32> {
    let before = number(before)?;
    let after = number(after)?;
    Some(distance(before, after, modulus))
}

/// The height of one item, from the vertical centres of the visible ones.
///
/// The median gap is used rather than the mean because one oddly tall row (or
/// a stray label between items) should not move the estimate.
pub fn item_height(centers: &[i32]) -> Option<i32> {
    let mut sorted = centers.to_vec();
    sorted.sort_unstable();
    let mut gaps: Vec<i32> = sorted
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .filter(|gap| *gap > 2)
        .collect();
    if gaps.is_empty() {
        return None;
    }
    gaps.sort_unstable();
    Some(gaps[gaps.len() / 2])
}

/// One item as the wheel currently shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub text: String,
    pub center_x: i32,
    pub center_y: i32,
    /// The accessibility layer says this item is the selected one.
    pub selected: bool,
    /// Its own height, when known; `0` when not.
    pub height: i32,
}

impl Item {
    /// An item known only by where its centre is.
    pub fn new(text: impl Into<String>, center_x: i32, center_y: i32) -> Self {
        Self {
            text: text.into(),
            center_x,
            center_y,
            selected: false,
            height: 0,
        }
    }
}

/// Which visible item the wheel has selected.
///
/// Three signals, in decreasing reliability: an item the accessibility layer
/// marks selected; otherwise the item whose own vertical span covers the
/// midline. The second is deliberately allowed to answer `-1` — some wheels
/// report only the rows above and below the selected one, and inventing a
/// middle row from two neighbours is guesswork the caller should see as a
/// gap rather than as a reading.
pub fn selected_index(items: &[Item], center_y: i32, height: Option<i32>) -> Option<usize> {
    let marked: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.selected)
        .map(|(index, _)| index)
        .collect();
    if let [single] = marked.as_slice() {
        return Some(*single);
    }
    if !items.is_empty() && items.iter().all(|item| item.height > 0) {
        return items
            .iter()
            .enumerate()
            .filter(|(_, item)| (item.center_y - center_y).abs() * 2 <= item.height)
            .min_by_key(|(_, item)| (item.center_y - center_y).abs())
            .map(|(index, _)| index);
    }
    selected(&items.iter().map(|item| item.center_y).collect::<Vec<_>>(), center_y, height)
}

/// The item on the midline: nearest centre, or `None` when nothing is close
/// enough to be the row a person would say is selected.
pub fn selected(centers: &[i32], center_y: i32, height: Option<i32>) -> Option<usize> {
    let best = centers
        .iter()
        .enumerate()
        .min_by_key(|(_, center)| (*center - center_y).abs())
        .map(|(index, _)| index)?;
    if let Some(height) = height {
        if (centers[best] - center_y).abs() > height * 3 / 4 {
            return None;
        }
    }
    Some(best)
}

/// The value of the row between two reported rows when the middle row was not
/// reported at all.
///
/// Some wheels only expose the rows above and below the selection. When the
/// two differ by two the middle is simply their average; otherwise only a
/// wrap-around is accepted (`59` above `01` is `00`), because any other gap
/// would mean guessing at how many rows are hidden.
pub fn between(above: &str, below: &str) -> Option<String> {
    let a = number(Some(above))?;
    let b = number(Some(below))?;
    let middle = if (b - a).abs() == 2 {
        (a + b) / 2
    } else if b < a && b <= 2 {
        if b >= 1 {
            b - 1
        } else {
            a + 1
        }
    } else if a < b && a <= 2 {
        if a >= 1 {
            a - 1
        } else {
            b + 1
        }
    } else {
        return None;
    };
    if middle < 0 {
        return None;
    }
    let above = normalize(above);
    let below = normalize(below);
    let token = *standalone_numbers(&above).first()?;
    let other = standalone_numbers(&below)
        .first()
        .copied()
        .unwrap_or_default();
    // A zero-padded wheel (`09`, `01`) stays padded.
    let padded = [token, other]
        .iter()
        .any(|digit| digit.len() > 1 && digit.starts_with('0'));
    let replacement = if padded {
        format!("{middle:0>width$}", width = token.len())
    } else {
        middle.to_string()
    };
    Some(above.replacen(token, &replacement, 1))
}

/// The run of items that belongs to the same column as `anchor`.
///
/// A wheel sits on a screen beside other wheels and under titles and buttons.
/// Starting from the selected row, neighbours are kept while they are about
/// one row apart, so the heading above the column and the button below it are
/// not mistaken for items.
pub fn column(items: &[Item], anchor: usize) -> Vec<Item> {
    let (start, end) = column_range(
        items.len(),
        &items.iter().map(|item| item.center_y).collect::<Vec<_>>(),
        anchor,
    );
    items[start..end].to_vec()
}

/// The half-open index range [`column`] keeps, for callers with parallel data.
///
/// The run is always contiguous, so a caller holding a second array aligned to
/// the same rows (each row's own bounds, say) slices both by this one range
/// instead of re-identifying items by value. `ys` is the rows' centre lines,
/// which is all the run rule looks at.
pub fn column_range(len: usize, ys: &[i32], anchor: usize) -> (usize, usize) {
    let all = (0, len);
    if len < 3 || anchor >= len || ys.len() != len {
        return all;
    }
    let spacing = [
        anchor.checked_sub(1).map(|above| ys[anchor] - ys[above]),
        ys.get(anchor + 1).map(|below| below - ys[anchor]),
    ]
    .into_iter()
    .flatten()
    .filter(|gap| *gap > 2)
    .min();
    let Some(spacing) = spacing else {
        return all;
    };
    let even =
        |gap: i32| (gap as f64) > spacing as f64 * 0.4 && (gap as f64) < spacing as f64 * 1.8;
    let mut low = anchor;
    while low > 0 && even(ys[low] - ys[low - 1]) {
        low -= 1;
    }
    let mut high = anchor;
    while high + 1 < len && even(ys[high + 1] - ys[high]) {
        high += 1;
    }
    (low, high + 1)
}

/// How many steps forward (negative for backward) reach `target`.
///
/// A target visible on the wheel is a matter of position. A numeric target
/// that is not visible is a matter of arithmetic, taking the short way round
/// when the loop length is visible or was seen earlier. Anything else — a text
/// wheel whose target is off-screen — returns `None`, which tells the planner
/// to scan instead.
pub fn steps_to(
    visible: &[String],
    selected: Option<usize>,
    target: &str,
    increasing_forward: bool,
    known_modulus: Option<i32>,
) -> Option<i32> {
    let selected = selected.filter(|index| *index < visible.len())?;
    if let Some(at) = visible.iter().position(|item| matches(Some(item), target)) {
        return i32::try_from(at).ok().map(|at| at - selected as i32);
    }
    let target = numeric_target(target)?;
    let current = number(Some(visible.get(selected)?))?;
    let delta = distance(current, target, modulus(visible).or(known_modulus));
    Some(if increasing_forward { delta } else { -delta })
}

/// The drag distances that turn `steps` items, each `item_height` tall.
///
/// One drag longer than `max_drag` would leave the column (and a drag that
/// long is also a fling), so a long move is split into several drags of at
/// most the visible span.
pub fn drags(steps: i32, item_height: i32, max_drag: i32) -> Vec<i32> {
    if steps == 0 || item_height <= 0 {
        return Vec::new();
    }
    let per_drag = (max_drag / item_height).max(1);
    let sign = if steps > 0 { 1 } else { -1 };
    let mut out = Vec::new();
    let mut left = steps.abs();
    while left > 0 {
        let count = left.min(per_drag);
        out.push(sign * count * item_height);
        left -= count;
    }
    out
}

/// The most characters one item may have and still be read as a wheel value.
const MAX_ITEM_CHARS: usize = 8;

/// Whether this run of items looks like a wheel.
///
/// All of these matter; the first two alone do not distinguish a wheel from
/// any short list:
///
/// * 3–9 items — a wheel shows a handful, a list scrolls through many;
/// * every item short and non-empty — values, not sentences;
/// * every item different — a wheel never repeats a value in view;
/// * every item centred on the same vertical line, within 15% of the width —
///   this is what excludes a settings page, whose labels are left-aligned;
/// * the items strictly descend the screen;
/// * the gaps are even (largest under 1.5× the smallest) — this is what
///   excludes a chat list, whose rows have ragged heights.
pub fn looks_like_wheel(items: &[Item], left: i32, right: i32) -> bool {
    if !(3..=9).contains(&items.len()) || right <= left {
        return false;
    }
    let texts: Vec<String> = items.iter().map(|item| normalize(&item.text)).collect();
    if texts
        .iter()
        .any(|text| text.is_empty() || text.chars().count() > MAX_ITEM_CHARS)
    {
        return false;
    }
    let unique: std::collections::HashSet<&String> = texts.iter().collect();
    if unique.len() != texts.len() {
        return false;
    }
    let midline = (left as f64 + right as f64) / 2.0;
    let tolerance = (right - left) as f64 * 0.15;
    if items
        .iter()
        .any(|item| (item.center_x as f64 - midline).abs() > tolerance)
    {
        return false;
    }
    let ys: Vec<i32> = items.iter().map(|item| item.center_y).collect();
    if ys.windows(2).any(|pair| pair[1] <= pair[0]) {
        return false;
    }
    let gaps: Vec<i32> = ys.windows(2).map(|pair| pair[1] - pair[0]).collect();
    let smallest = gaps.iter().copied().min().unwrap_or(0);
    let largest = gaps.iter().copied().max().unwrap_or(0);
    (largest as f64) < (smallest as f64) * 1.5
}

/// How tall one item actually is, from a drag and the number of items it turned.
///
/// `None` when the drag turned nothing — there is no ratio to learn from — and
/// when the answer is absurdly small, which means the reading moved for some
/// other reason.
pub fn calibrated(dragged: i32, moved: i32) -> Option<i32> {
    if moved == 0 {
        return None;
    }
    let height = (dragged as f64 / moved as f64).round() as i32;
    (height.abs() > 2).then_some(height.abs())
}

/// What the planner wants done next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WheelStep {
    /// The wheel now shows the target.
    Done,
    /// Scroll `count` times in this direction.
    Act { forward: bool, count: i32 },
    /// Stop, for one of the [`WheelStop`] reasons.
    Stop(WheelStop),
}

/// Why the planner stopped without reaching the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelStop {
    /// This direction has nothing more, and the target is not there.
    AtEnd,
    /// The wheel moved nothing: a looping wheel has no end, so this is a
    /// control that does not respond to the scroll action.
    NotMoving,
    /// The step budget ran out.
    Cap,
    /// The action overshoots the target every time; a drag is needed.
    Overshoot,
    /// The current value cannot be read.
    UnknownValue,
}

impl WheelStop {
    /// The code the tool reports.
    pub fn code(self) -> &'static str {
        match self {
            Self::AtEnd => "wheel_at_end",
            Self::NotMoving => "wheel_not_moving",
            Self::Cap => "wheel_not_reached",
            Self::Overshoot => "wheel_not_reached",
            Self::UnknownValue => "wheel_unreadable",
        }
    }
}

/// The next accessibility scroll for a wheel, batch by batch.
///
/// One `ACTION_SCROLL_FORWARD` does not necessarily move one item (a list
/// scrolls a page), so every batch is followed by a fresh reading and the
/// direction is recomputed from it. That is also what catches "forward" being
/// the direction of *smaller* values on some wheels: the first reading that
/// moves tells the planner which way is which.
#[derive(Debug)]
pub struct WheelSteps {
    target: String,
    cap: i32,
    burst: i32,
    actions: i32,
    numeric: Option<i32>,
    increasing_forward: bool,
    calibrated_direction: bool,
    modulus: Option<i32>,
    last: Option<String>,
    last_action: Option<(bool, i32)>,
    reversals: i32,
    scan_forward: bool,
    scan_reversed: bool,
}

impl WheelSteps {
    /// A planner for `target`, giving up after `cap` scroll actions.
    pub fn new(target: impl Into<String>, cap: i32) -> Self {
        let target = target.into();
        let numeric = numeric_target(&target);
        Self {
            target,
            cap,
            burst: 5,
            actions: 0,
            numeric,
            increasing_forward: true,
            calibrated_direction: false,
            modulus: None,
            last: None,
            last_action: None,
            reversals: 0,
            scan_forward: true,
            scan_reversed: false,
        }
    }

    /// How many scroll actions have been issued so far.
    pub fn actions(&self) -> i32 {
        self.actions
    }

    /// Given the selected value and the visible items, what to do next.
    pub fn next(&mut self, current: Option<&str>, visible: &[String]) -> WheelStep {
        if matches(current, &self.target) {
            return WheelStep::Done;
        }
        let Some(current) = current else {
            return WheelStep::Stop(WheelStop::UnknownValue);
        };
        if let Some(modulus) = modulus(visible) {
            self.modulus = Some(modulus);
        }
        let previous = self.last_action;
        let unchanged = previous.is_some() && self.last.as_deref() == Some(current);
        if self.actions >= self.cap {
            return WheelStep::Stop(WheelStop::Cap);
        }
        if let (Some(target), Some(_)) = (self.numeric, number(Some(current))) {
            if let Some((forward, _)) = previous {
                if unchanged {
                    // A looping wheel has no end, so a scroll that changed
                    // nothing is a control that ignores the action.
                    return WheelStep::Stop(if self.modulus.is_some() {
                        WheelStop::NotMoving
                    } else {
                        WheelStop::AtEnd
                    });
                }
                if !self.calibrated_direction {
                    if let Some(moved) = moved(self.last.as_deref(), Some(current), self.modulus) {
                        if moved != 0 {
                            self.calibrated_direction = true;
                            if (moved > 0) != forward {
                                self.increasing_forward = false;
                            }
                        }
                    }
                }
            }
            let from = number(Some(current)).unwrap_or(0);
            let delta = distance(from, target, self.modulus);
            let delta = if self.increasing_forward {
                delta
            } else {
                -delta
            };
            let forward = delta > 0;
            let turned = previous.is_some_and(|(was, _)| was != forward);
            if turned {
                self.reversals += 1;
                if self.reversals > 3 {
                    return WheelStep::Stop(WheelStop::Overshoot);
                }
            }
            // Coming back across the target: one at a time, so it cannot
            // overshoot again.
            let count = if turned {
                1
            } else {
                delta.abs().min(self.burst)
            };
            return self.act(forward, count.min(self.cap - self.actions).max(0), current);
        }
        let selected = visible.iter().position(|item| item == current);
        let at = visible
            .iter()
            .position(|item| matches(Some(item), &self.target));
        if let (Some(selected), Some(at)) = (selected, at) {
            let (selected, at) = (selected as i32, at as i32);
            return self.act(
                at > selected,
                (at - selected)
                    .abs()
                    .min(self.burst)
                    .min(self.cap - self.actions),
                current,
            );
        }
        if unchanged {
            if self.scan_reversed {
                return WheelStep::Stop(WheelStop::AtEnd);
            }
            self.scan_reversed = true;
            self.scan_forward = !self.scan_forward;
        }
        self.act(self.scan_forward, 1, current)
    }

    fn act(&mut self, forward: bool, count: i32, current: &str) -> WheelStep {
        let count = count.max(1);
        self.last = Some(current.to_string());
        self.last_action = Some((forward, count));
        self.actions += count;
        WheelStep::Act { forward, count }
    }
}

/// What the drag planner wants done next.
#[derive(Debug, Clone, PartialEq)]
pub enum WheelDrag {
    /// The wheel now shows the target.
    Done,
    /// Drag `steps` items (positive is fingers up = forward), scaled by
    /// `boost`; `slow` keeps the finger down instead of flicking.
    Drag { steps: i32, boost: f64, slow: bool },
    /// Stop, for one of the [`WheelStop`] reasons.
    Stop(WheelStop),
}

/// The next drag for a wheel the accessibility API cannot scroll.
///
/// The same feedback loop as [`WheelSteps`], plus what only dragging needs:
/// the height of an item is learned from how far a drag actually turned the
/// wheel, and a drag that changed nothing is retried differently (faster,
/// further) before it is called a failure — a wheel that ignored one drag may
/// still move for another.
#[derive(Debug)]
pub struct WheelDrags {
    target: String,
    numeric: Option<i32>,
    item_height: i32,
    started: bool,
    increasing_forward: bool,
    direction_known: bool,
    modulus: Option<i32>,
    last: Option<String>,
    last_drag: Option<(i32, bool)>,
    last_scanning: bool,
    scan_forward: bool,
    scan_reversed: bool,
    moves: i32,
    still: i32,
    max_moves: i32,
    max_still: i32,
}

impl WheelDrags {
    /// A planner for `target`, starting from a guessed row height.
    pub fn new(target: impl Into<String>, row_height: i32) -> Self {
        let target = target.into();
        let numeric = numeric_target(&target);
        Self {
            target,
            numeric,
            item_height: row_height.max(1),
            started: false,
            increasing_forward: true,
            direction_known: false,
            modulus: None,
            last: None,
            last_drag: None,
            last_scanning: false,
            scan_forward: true,
            scan_reversed: false,
            moves: 0,
            still: 0,
            max_moves: 5,
            max_still: 3,
        }
    }

    /// The current estimate of an item's height in pixels.
    pub fn item_height(&self) -> i32 {
        self.item_height
    }

    /// Given the visible items, the selected index and how far the last drag
    /// actually travelled, what to do next.
    ///
    /// `centers` are the items' vertical centres; `dragged` is the pixels the
    /// finger moved on the last drag, positive up.
    pub fn next(
        &mut self,
        items: &[String],
        selected: Option<usize>,
        centers: &[i32],
        dragged: i32,
    ) -> WheelDrag {
        let current = selected
            .and_then(|index| items.get(index))
            .map(String::as_str);
        if matches(current, &self.target) {
            return WheelDrag::Done;
        }
        if items.is_empty() {
            return WheelDrag::Stop(WheelStop::UnknownValue);
        }
        if !self.started {
            self.started = true;
            if let Some(height) = item_height(centers) {
                self.item_height = height;
            }
        }
        if let Some(modulus) = modulus(items) {
            self.modulus = Some(modulus);
        }
        if let Some((steps, _)) = self.last_drag {
            if current != self.last.as_deref() {
                self.still = 0;
                self.moves += 1;
                if let Some(moved) = moved(self.last.as_deref(), current, self.modulus) {
                    if moved != 0 {
                        if !self.direction_known && self.numeric.is_some() {
                            self.direction_known = true;
                            if (moved > 0) != (dragged > 0) {
                                self.increasing_forward = false;
                            }
                        }
                        if let Some(height) = calibrated(dragged, moved) {
                            self.item_height = height;
                        }
                    }
                }
            } else if self.at_end(items, selected, steps > 0) {
                if !self.last_scanning || self.scan_reversed {
                    return WheelDrag::Stop(WheelStop::AtEnd);
                }
                self.scan_reversed = true;
                self.scan_forward = !self.scan_forward;
            } else {
                self.still += 1;
            }
        }
        if self.still >= self.max_still {
            return WheelDrag::Stop(WheelStop::NotMoving);
        }
        if self.moves >= self.max_moves {
            return WheelDrag::Stop(WheelStop::Cap);
        }
        let planned =
            steps_to(items, selected, &self.target, self.increasing_forward, self.modulus);
        // Arithmetic says it is already there but the reading disagrees: the
        // value is not on this wheel at all (`75` in a 0–59 column).
        if planned == Some(0) {
            return WheelDrag::Stop(WheelStop::Cap);
        }
        self.last_scanning = planned.is_none();
        let steps = planned.unwrap_or_else(|| {
            let span = i32::try_from(items.len().saturating_sub(1))
                .unwrap_or(1)
                .max(1);
            if self.scan_forward {
                span
            } else {
                -span
            }
        });
        self.last = current.map(str::to_string);
        // A drag that changed nothing is retried faster and, when it was only
        // one item short, further; a large drag is not stretched, or it would
        // overshoot the moment it works.
        let boost = if steps.abs() <= 1 {
            1.0 + 0.6 * f64::from(self.still)
        } else {
            1.0
        };
        let slow = self.still % 2 == 0;
        self.last_drag = Some((steps, slow));
        WheelDrag::Drag { steps, boost, slow }
    }

    /// Whether `forward` has run out of items.
    ///
    /// Only meaningful without a loop: a looping wheel has no end, so a drag
    /// that moved nothing is a control that ignored the gesture.
    fn at_end(&self, items: &[String], selected: Option<usize>, forward: bool) -> bool {
        if self.modulus.is_some() || items.len() < 2 {
            return false;
        }
        match selected {
            Some(index) if forward => index + 1 == items.len(),
            Some(0) if !forward => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str, x: i32, y: i32) -> Item {
        Item::new(text, x, y)
    }

    #[test]
    fn normalisation_folds_full_width_digits_and_case() {
        assert_eq!(normalize("  ２０２６年 "), "2026年");
        assert_eq!(normalize("SeLeCtEd"), "selected");
        assert_eq!(normalize(" 9 时 "), "9时");
    }

    #[test]
    fn a_label_reads_as_the_number_it_shows() {
        assert_eq!(number(Some("09")), Some(9));
        assert_eq!(number(Some("9时")), Some(9));
        assert_eq!(number(Some("2026年")), Some(2026));
        assert_eq!(number(Some("第3周")), Some(3));
        assert_eq!(number(Some("已选中，21")), Some(21));
        assert_eq!(number(Some("Selected, 21")), Some(21));
        assert_eq!(number(Some("abc")), None);
    }

    #[test]
    fn several_numbers_with_no_leading_one_are_not_a_value() {
        // Two numbers, and the first is too far into the label to be the value.
        assert_eq!(number(Some("现在是北京时间 21 和 22")), None);
        // One number, so it is the value even with words in front of it.
        assert_eq!(number(Some("北京时间 21")), Some(21));
    }

    #[test]
    fn a_status_word_is_stripped_from_a_numeric_label_only() {
        assert_eq!(clean(Some("已选中，21")).as_deref(), Some("21"));
        assert_eq!(clean(Some("Selected, 21")).as_deref(), Some("21"));
        // A text label has no unique numeric part; it is quoted whole.
        assert_eq!(clean(Some("北京, 市")).as_deref(), Some("北京, 市"));
        assert_eq!(clean(Some("21")).as_deref(), Some("21"));
        assert_eq!(clean(Some("   ")), None);
    }

    #[test]
    fn a_numeric_target_only_counts_when_it_is_only_a_number() {
        assert_eq!(numeric_target("09"), Some(9));
        assert_eq!(numeric_target("２０２６"), Some(2026));
        assert_eq!(numeric_target("40分"), None);
        assert_eq!(numeric_target("PM"), None);
    }

    #[test]
    fn matching_is_numeric_for_numbers_and_affix_for_words() {
        assert!(matches(Some("09"), "9"));
        assert!(matches(Some("已选中，21"), "21"));
        assert!(matches(Some("40分"), "40"));
        assert!(matches(Some("北京市"), "北京"));
        assert!(matches(Some("Beijing, 已选中"), "beijing"));
        assert!(!matches(Some("21"), "22"));
        assert!(!matches(None, "21"));
    }

    #[test]
    fn a_clock_column_visible_in_the_middle_of_the_wrap_has_its_length() {
        let column: Vec<String> = ["58", "59", "00", "01"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(modulus(&column), Some(60));
    }

    #[test]
    fn a_column_with_no_wrap_or_two_wraps_has_no_length() {
        let rising: Vec<String> = ["1", "2", "3"].iter().map(|s| s.to_string()).collect();
        assert_eq!(modulus(&rising), None);
        let two_wraps: Vec<String> = ["58", "00", "20", "00"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(modulus(&two_wraps), None);
        let words: Vec<String> = ["AM", "PM"].iter().map(|s| s.to_string()).collect();
        assert_eq!(modulus(&words), None);
    }

    #[test]
    fn a_loop_takes_the_short_way_round() {
        assert_eq!(distance(59, 0, Some(60)), 1);
        assert_eq!(distance(0, 59, Some(60)), -1);
        assert_eq!(distance(10, 20, Some(60)), 10);
        assert_eq!(distance(10, 50, Some(60)), -20);
        // Without a loop the answer is the plain difference.
        assert_eq!(distance(10, 50, None), 40);
    }

    #[test]
    fn a_move_across_the_wrap_is_one_item_not_fifty_nine() {
        assert_eq!(moved(Some("59"), Some("00"), Some(60)), Some(1));
        assert_eq!(moved(Some("00"), Some("59"), Some(60)), Some(-1));
        assert_eq!(moved(Some("AM"), Some("PM"), None), None);
    }

    #[test]
    fn item_height_is_the_median_gap() {
        assert_eq!(item_height(&[100, 180, 260, 340]), Some(80));
        // One outlier gap does not move the median.
        assert_eq!(item_height(&[100, 180, 260, 400]), Some(80));
        assert_eq!(item_height(&[100]), None);
        // Gaps of two pixels or less are layout noise, not rows.
        assert_eq!(item_height(&[100, 101, 102]), None);
    }

    #[test]
    fn the_selected_item_is_the_one_marked_selected() {
        let mut marked = item("21", 540, 1200);
        marked.selected = true;
        let items = vec![item("20", 540, 1120), marked, item("22", 540, 1280)];
        assert_eq!(selected_index(&items, 1200, Some(80)), Some(1));
    }

    #[test]
    fn without_a_marker_the_item_covering_the_midline_is_selected() {
        let mut items = vec![
            item("20", 540, 1120),
            item("21", 540, 1200),
            item("22", 540, 1280),
        ];
        for entry in &mut items {
            entry.height = 80;
        }
        assert_eq!(selected_index(&items, 1200, Some(80)), Some(1));
        // No item covers the midline: this wheel reports only the rows around
        // the gap. Saying "nothing" is honest; inventing a neighbour is not.
        let mut narrow = items.clone();
        for entry in &mut narrow {
            entry.height = 40;
        }
        assert_eq!(selected_index(&narrow, 1160, Some(40)), None);
    }

    #[test]
    fn the_row_between_two_reported_rows_is_their_average() {
        assert_eq!(between("20", "22").as_deref(), Some("21"));
        // Across the wrap, padded like the rows around it.
        assert_eq!(between("59", "01").as_deref(), Some("00"));
        assert_eq!(between("12", "02").as_deref(), Some("01"));
        // A gap that is neither two nor a wrap is not guessed at.
        assert_eq!(between("10", "40"), None);
        assert_eq!(between("AM", "PM"), None);
    }

    #[test]
    fn a_column_stops_at_the_heading_and_the_button() {
        let items = vec![
            item("Date", 540, 900),
            item("20", 540, 1120),
            item("21", 540, 1200),
            item("22", 540, 1280),
            item("OK", 540, 1500),
        ];
        let column = column(&items, 1);
        assert_eq!(column.len(), 3, "{column:?}");
        assert_eq!(column[1].text, "21");
    }

    #[test]
    fn an_item_on_the_midline_is_selected_by_distance() {
        assert_eq!(selected(&[1100, 1180, 1260], 1180, Some(80)), Some(1));
        // Forty pixels is within three quarters of a row.
        assert_eq!(selected(&[1100, 1180, 1260], 1140, Some(80)), Some(0));
        // Nothing is within three quarters of a row of this midline.
        assert_eq!(selected(&[1100, 1180, 1260], 1020, Some(80)), None);
    }

    #[test]
    fn steps_come_from_position_when_the_value_is_visible() {
        let visible: Vec<String> = ["19", "20", "21"].iter().map(|s| s.to_string()).collect();
        assert_eq!(steps_to(&visible, Some(1), "21", true, None), Some(1));
        assert_eq!(steps_to(&visible, Some(2), "19", true, None), Some(-2));
    }

    #[test]
    fn steps_come_from_arithmetic_when_the_value_is_not_visible() {
        let visible: Vec<String> = ["58", "59", "00"].iter().map(|s| s.to_string()).collect();
        // 59 → 30 the short way is backwards; the loop was read from the column.
        assert_eq!(steps_to(&visible, Some(1), "30", true, None), Some(-29));
        // 00 → 05 forward.
        assert_eq!(steps_to(&visible, Some(2), "05", true, None), Some(5));
    }

    #[test]
    fn a_text_target_that_is_not_visible_has_no_step_count() {
        let visible: Vec<String> = ["Beijing", "Shanghai"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(steps_to(&visible, Some(0), "Shenzhen", true, None), None);
    }

    #[test]
    fn a_long_drag_is_split_into_visible_ones() {
        assert_eq!(drags(3, 80, 1000), vec![240]);
        // Five rows fit in one 400 px drag, so twenty rows is four drags.
        assert_eq!(drags(20, 80, 400), vec![400, 400, 400, 400]);
        assert_eq!(drags(-10, 80, 400), vec![-400, -400]);
        assert_eq!(drags(0, 80, 400), Vec::<i32>::new());
        assert_eq!(drags(3, 0, 400), Vec::<i32>::new());
    }

    #[test]
    fn a_centred_even_run_of_short_values_is_a_wheel() {
        let items: Vec<Item> = ["19", "20", "21", "22"]
            .iter()
            .enumerate()
            .map(|(i, text)| item(text, 540 + i as i32 * 2, 1100 + i as i32 * 80))
            .collect();
        assert!(looks_like_wheel(&items, 400, 680));
    }

    #[test]
    fn a_left_aligned_settings_list_is_not_a_wheel() {
        // Same labels, same spacing — but every row starts at the left edge,
        // so its centre is not on the column's midline.
        let items: Vec<Item> = ["19", "20", "21", "22"]
            .iter()
            .enumerate()
            .map(|(i, text)| item(text, 420, 1100 + i as i32 * 80))
            .collect();
        assert!(!looks_like_wheel(&items, 400, 680));
    }

    #[test]
    fn a_run_with_uneven_gaps_is_not_a_wheel() {
        let items = vec![
            item("19", 540, 1100),
            item("20", 540, 1180),
            item("21", 540, 1260),
            // A gap twice the others: the row after it is not part of the run.
            item("22", 540, 1420),
        ];
        assert!(!looks_like_wheel(&items, 400, 680));
    }

    #[test]
    fn a_wheel_needs_three_items_and_no_more_than_nine() {
        let short: Vec<Item> = (0..2).map(|i| item("1", 540, 1100 + i * 80)).collect();
        assert!(!looks_like_wheel(&short, 400, 680));
        let long: Vec<Item> = (0..10)
            .map(|i| item(&i.to_string(), 540, 1100 + i * 80))
            .collect();
        assert!(!looks_like_wheel(&long, 400, 680));
    }

    #[test]
    fn repeated_or_overlong_values_are_not_a_wheel() {
        let repeated = vec![
            item("AM", 540, 1100),
            item("AM", 540, 1180),
            item("AM", 540, 1260),
        ];
        assert!(!looks_like_wheel(&repeated, 400, 680));
        let long = vec![
            item("September", 540, 1100),
            item("October", 540, 1180),
            item("November", 540, 1260),
        ];
        assert!(!looks_like_wheel(&long, 400, 680));
    }

    #[test]
    fn a_drag_calibrates_the_height_of_one_item() {
        assert_eq!(calibrated(160, 2), Some(80));
        assert_eq!(calibrated(-160, -2), Some(80));
        assert_eq!(calibrated(160, 0), None);
        assert_eq!(calibrated(4, 3), None);
    }

    // ── Planners ───────────────────────────────────────────────────────────

    fn visible(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn a_wheel_already_showing_the_value_needs_no_steps() {
        let mut planner = WheelSteps::new("21", 80);
        assert_eq!(planner.next(Some("21"), &visible(&["20", "21", "22"])), WheelStep::Done);
        assert_eq!(planner.actions(), 0);
    }

    #[test]
    fn a_reading_that_cannot_be_parsed_is_reported_not_guessed() {
        let mut planner = WheelSteps::new("21", 80);
        assert_eq!(
            planner.next(None, &visible(&["20", "21"])),
            WheelStep::Stop(WheelStop::UnknownValue)
        );
    }

    #[test]
    fn a_numeric_wheel_scrolls_forward_by_the_difference() {
        let mut planner = WheelSteps::new("30", 80);
        // 20 → 30 is ten items forward, issued in a batch of five.
        assert_eq!(
            planner.next(Some("20"), &visible(&["18", "19", "20", "21"])),
            WheelStep::Act { forward: true, count: 5 }
        );
        assert_eq!(
            planner.next(Some("25"), &visible(&["23", "24", "25", "26"])),
            WheelStep::Act { forward: true, count: 5 }
        );
    }

    #[test]
    fn a_scroll_that_changes_nothing_ends_the_run() {
        let mut planner = WheelSteps::new("30", 80);
        let _ = planner.next(Some("20"), &visible(&["18", "19", "20", "21"]));
        // The same value came back: on a non-looping wheel that is the end.
        assert_eq!(
            planner.next(Some("20"), &visible(&["18", "19", "20", "21"])),
            WheelStep::Stop(WheelStop::AtEnd)
        );
    }

    #[test]
    fn a_looping_wheel_that_does_not_move_is_not_at_its_end() {
        let mut planner = WheelSteps::new("30", 80);
        let column = visible(&["58", "59", "00", "01"]);
        let _ = planner.next(Some("59"), &column);
        assert_eq!(planner.next(Some("59"), &column), WheelStep::Stop(WheelStop::NotMoving));
    }

    #[test]
    fn a_wheel_whose_forward_is_smaller_is_calibrated_from_the_reading() {
        let mut planner = WheelSteps::new("10", 80);
        // No calibration yet, so 10 below 30 reads as backwards.
        assert_eq!(
            planner.next(Some("30"), &visible(&["29", "30", "31"])),
            WheelStep::Act { forward: false, count: 5 }
        );
        // That backward press went 30 → 35: backward is *larger* here, so
        // forward is smaller, and the same target is now reached forward. The
        // direction is a reversal, so it goes one at a time until it is sure.
        assert_eq!(
            planner.next(Some("35"), &visible(&["34", "35", "36"])),
            WheelStep::Act { forward: true, count: 1 }
        );
    }

    #[test]
    fn a_repeated_overshoot_is_handed_to_the_drag_planner() {
        let mut planner = WheelSteps::new("50", 80);
        // Alternate sides four times without ever landing on the value.
        let _ = planner.next(Some("40"), &visible(&["39", "40", "41"]));
        let _ = planner.next(Some("55"), &visible(&["54", "55", "56"]));
        let _ = planner.next(Some("45"), &visible(&["44", "45", "46"]));
        let _ = planner.next(Some("51"), &visible(&["50", "51", "52"]));
        let outcome = planner.next(Some("49"), &visible(&["48", "49", "50"]));
        assert_eq!(outcome, WheelStep::Stop(WheelStop::Overshoot));
    }

    #[test]
    fn a_text_wheel_scans_one_step_at_a_time() {
        let mut planner = WheelSteps::new("Shenzhen", 80);
        assert_eq!(
            planner.next(Some("Beijing"), &visible(&["Beijing", "Wuhan"])),
            WheelStep::Act { forward: true, count: 1 }
        );
    }

    #[test]
    fn the_step_budget_is_respected() {
        let mut planner = WheelSteps::new("999", 3);
        let _ = planner.next(Some("1"), &visible(&["0", "1", "2"]));
        let _ = planner.next(Some("2"), &visible(&["1", "2", "3"]));
        assert_eq!(
            planner.next(Some("3"), &visible(&["2", "3", "4"])),
            WheelStep::Stop(WheelStop::Cap)
        );
    }

    #[test]
    fn the_drag_planner_finishes_when_the_value_arrives() {
        let mut planner = WheelDrags::new("21", 80);
        assert_eq!(
            planner.next(&visible(&["20", "21", "22"]), Some(1), &[1120, 1200, 1280], 0),
            WheelDrag::Done
        );
    }

    #[test]
    fn the_drag_planner_learns_the_row_height_from_what_the_drag_turned() {
        let mut planner = WheelDrags::new("30", 80);
        let first = planner.next(&visible(&["20", "21", "22"]), Some(0), &[1120, 1200, 1280], 0);
        assert!(matches!(first, WheelDrag::Drag { .. }), "{first:?}");
        // The drag went from 20 to 24 — four items in 240 px, so 60 px each.
        planner.next(&visible(&["22", "23", "24"]), Some(2), &[1120, 1200, 1280], 240);
        assert_eq!(planner.item_height(), 60);
        // The next one went 24 to 27: three items, so 80 px.
        planner.next(&visible(&["25", "26", "27"]), Some(2), &[1120, 1200, 1280], 240);
        assert_eq!(planner.item_height(), 80);
    }

    #[test]
    fn a_drag_that_changes_nothing_is_retried_before_it_fails() {
        // A looping column has no end, so "the value did not change" is not
        // "the target is past the last row" — it is a drag that did nothing,
        // and such a drag is worth retrying differently before giving up.
        let mut planner = WheelDrags::new("30", 80);
        let column = visible(&["58", "59", "00", "01"]);
        let mut last = WheelDrag::Done;
        for _ in 0..6 {
            last = planner.next(&column, Some(1), &[1120, 1200, 1280], 0);
            if matches!(last, WheelDrag::Stop(_)) {
                break;
            }
        }
        assert_eq!(last, WheelDrag::Stop(WheelStop::NotMoving));
    }

    #[test]
    fn a_value_that_is_not_on_the_wheel_stops_instead_of_circling() {
        let mut planner = WheelDrags::new("75", 80);
        let column = visible(&["58", "59", "00", "01"]);
        // The loop makes 75 look reachable, but the readings never say so;
        // after the move budget the planner stops rather than spinning.
        let mut last = WheelDrag::Drag {
            steps: 0,
            boost: 1.0,
            slow: true,
        };
        for _ in 0..12 {
            last = planner.next(&column, Some(1), &[1120, 1200, 1280], 0);
            if matches!(last, WheelDrag::Stop(_)) {
                break;
            }
        }
        assert!(matches!(last, WheelDrag::Stop(_)), "{last:?}");
    }

    #[test]
    fn a_stop_names_the_code_the_tool_reports() {
        assert_eq!(WheelStop::AtEnd.code(), "wheel_at_end");
        assert_eq!(WheelStop::NotMoving.code(), "wheel_not_moving");
        assert_eq!(WheelStop::UnknownValue.code(), "wheel_unreadable");
        assert_eq!(WheelStop::Cap.code(), "wheel_not_reached");
        assert_eq!(WheelStop::Overshoot.code(), "wheel_not_reached");
    }
}
