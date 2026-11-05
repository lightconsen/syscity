//! Merging what the accessibility tree and OCR each read on one screen.

use super::*;

/// How many OCR lines may sit within this many pixels of each other before
/// they are the same line.
///
/// OCR re-reads the same glyphs on consecutive screens — a header, a sticky
/// button — and hands them back a pixel or two apart. Anything within one
/// character's height of a line already taken is that line.
pub(crate) const OCR_SAME_LINE_PX: i32 = 8;

/// Budget for `android_collect`'s collected text, in characters.
///
/// A long chat is unbounded, and the tail of it is the part that matters least
/// to a question asked about it. The budget is the tool's own rather than
/// `EXTRACT_MAX_CHARS`: that one counts structured rows, this counts lines the
/// model will read in order, and hitting it has to stop the *scroll* — reading
/// more screens that would be dropped is pure cost.
pub const COLLECT_MAX_CHARS: usize = 50_000;

/// One readable thing on a screen, wherever it came from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReadLine {
    pub(super) text: String,
    /// Centre among the screen's elements, or the line's own when only OCR saw
    /// it. Used to order the two sources against each other.
    pub(super) center: (i32, i32),
    pub(super) source: LineSource,
}

/// Which reader produced a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineSource {
    Tree,
    Ocr,
}

impl LineSource {
    /// How the line is reported back.
    pub(super) fn tag(self) -> &'static str {
        match self {
            Self::Tree => "tree",
            Self::Ocr => "ocr",
        }
    }
}

/// What one screen read produced.
#[derive(Debug, Default, Clone)]
pub(crate) struct ScreenRead {
    pub(super) lines: Vec<ReadLine>,
    /// The sources that contributed to `lines`.
    pub(super) tree: bool,
    pub(super) ocr: bool,
}

impl ScreenRead {
    /// The one word the caller is told: tree, ocr, mixed — or none.
    pub(super) fn source(&self) -> &'static str {
        match (self.tree, self.ocr) {
            (true, true) => "mixed",
            (true, false) => "tree",
            (false, true) => "ocr",
            (false, false) => "none",
        }
    }
}

/// Merge the tree's labels with the OCR lines of the same screen, in reading
/// order.
///
/// The two are not alternatives: a chat screen exposes its container and its
/// input bar to the tree while the messages are drawn, so the *tree alone* says
/// "there is a list" and the *text alone* says nothing about what is scrollable.
/// Taking both, ordered by where they sit, gives a person's reading of the
/// screen.
///
/// A line the two both saw is taken once, from the tree: the tree's label is
/// exact where OCR may have mangled it, and its index is something a later tap
/// can address. Overlap is judged by the OCR line's *own* centre, not the
/// tree's — the tree's centre is the element's, and an element that covers half
/// the screen (a list) has a centre nowhere near its text.
pub(crate) fn merge_screen(elements: &[UiElement], ocr: &[OcrLine]) -> ScreenRead {
    let mut lines: Vec<ReadLine> = Vec::new();
    let mut tree = false;
    for element in elements {
        let Some(label) = element_label(element) else {
            continue;
        };
        let text = label.trim();
        if text.is_empty() || lines.iter().any(|line| line.text == text) {
            continue;
        }
        tree = true;
        lines.push(ReadLine {
            text: text.to_string(),
            center: element.center,
            source: LineSource::Tree,
        });
    }
    let mut ocr_used = false;
    for line in ocr {
        let text = line.text.trim();
        if text.is_empty() {
            continue;
        }
        let covered = lines.iter().any(|seen| {
            (seen.center.1 - line.center.1).abs() <= OCR_SAME_LINE_PX
                && (seen.center.0 - line.center.0).abs() <= OCR_SAME_LINE_PX
        }) || lines.iter().any(|seen| seen.text == text);
        if covered {
            continue;
        }
        ocr_used = true;
        lines.push(ReadLine {
            text: text.to_string(),
            center: line.center,
            source: LineSource::Ocr,
        });
    }
    lines.sort_by_key(|line| (line.center.1, line.center.0));
    ScreenRead { lines, tree, ocr: ocr_used }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;

    fn ocr_line(text: &str, x: i32, y: i32) -> OcrLine {
        OcrLine {
            text: text.to_string(),
            center: (x, y),
            bounds: (x - 50, y - 10, x + 50, y + 10),
            confidence: 0.9,
        }
    }
    #[test]
    fn a_usable_screen_is_read_from_the_tree_alone() {
        let elements = vec![wheel_row(1, "Alice: hi", 500, 200)];
        let read = merge_screen(&elements, &[]);
        assert_eq!(read.source(), "tree");
        assert_eq!(read.lines.len(), 1);
        assert_eq!(read.lines[0].source, LineSource::Tree);
    }
    #[test]
    fn a_screen_the_tree_cannot_describe_is_read_from_ocr() {
        // The tree exposes only an anonymous container; the messages are drawn.
        let elements: Vec<UiElement> = Vec::new();
        let ocr = vec![ocr_line("Bob: are you there?", 500, 400)];
        let read = merge_screen(&elements, &ocr);
        assert_eq!(read.source(), "ocr");
        assert_eq!(read.lines[0].text, "Bob: are you there?");
    }
    #[test]
    fn both_readers_together_are_merged_in_reading_order() {
        // A chat: the tree names the header and the input bar, OCR sees the
        // messages between them.
        let elements = vec![
            wheel_row(1, "Messages", 500, 100),
            wheel_row(2, "Type a message", 500, 2100),
        ];
        let ocr = vec![
            ocr_line("Alice: hi", 500, 900),
            ocr_line("Bob: hello", 500, 1100),
        ];
        let read = merge_screen(&elements, &ocr);
        assert_eq!(read.source(), "mixed");
        let texts: Vec<&str> = read.lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(texts, vec!["Messages", "Alice: hi", "Bob: hello", "Type a message"]);
    }
    #[test]
    fn a_line_both_readers_saw_is_taken_once_from_the_tree() {
        let elements = vec![wheel_row(1, "Send", 500, 2000)];
        // OCR sees the same button, a pixel or two off.
        let ocr = vec![ocr_line("Send", 502, 2002)];
        let read = merge_screen(&elements, &ocr);
        assert_eq!(read.lines.len(), 1);
        assert_eq!(read.lines[0].source, LineSource::Tree);
        // And it is not called OCR, because no OCR-only line survived.
        assert_eq!(read.source(), "tree");
    }
    #[test]
    fn ocr_text_the_tree_saw_elsewhere_is_dropped_as_a_duplicate() {
        // A sticky header is redrawn on the next screen; matching on the text
        // keeps it from being collected twice.
        let elements = vec![wheel_row(1, "Chats", 500, 100)];
        let ocr = vec![ocr_line("Chats", 500, 700)];
        let read = merge_screen(&elements, &ocr);
        assert_eq!(read.lines.len(), 1);
        assert_eq!(read.lines[0].text, "Chats");
    }
    #[test]
    fn an_unlabelled_structural_element_contributes_no_line() {
        let mut list = wheel_row(1, "", 500, 1000);
        list.text = None;
        list.state.scrollable = true;
        let read = merge_screen(&[list], &[]);
        assert!(read.lines.is_empty());
        assert_eq!(read.source(), "none");
    }
    #[test]
    fn the_ocr_budget_stops_the_scroll() {
        // The budget is the tool's own, and it has to be big enough that a
        // chat worth collecting fits but a whole feed does not.
        assert!(COLLECT_MAX_CHARS >= 20_000, "{COLLECT_MAX_CHARS}");
        assert_eq!(COLLECT_MAX_CHARS, 50_000);
    }
}
