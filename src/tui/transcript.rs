//! Transcript model: what has been frozen into scrollback, and what is still
//! live in the bottom region.
//!
//! The TUI has exactly two places a line can be: the terminal's scrollback
//! (written once, never redrawn) or the fixed-height live region at the bottom
//! (redrawn every frame). This module owns the boundary between them and
//! nothing else — it holds no ratatui types, so the policy is unit-testable
//! without a terminal.
//!
//! The rule that keeps both correct: **a line is emitted exactly once, either
//! into the live region or into scrollback, never both.**
// INVARIANTS-NONE: presentation-layer bookkeeping; holds no shared state.

/// Which style family a line belongs to, resolved to an actual `Style` by the
/// rendering layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// A user's own message echo.
    User,
    /// Assistant prose.
    Assistant,
    /// Assistant reasoning / thinking.
    Reasoning,
    /// A tool invocation header.
    Tool,
    /// A tool result body.
    ToolResult,
    /// A line inside a fenced code block.
    Code,
    /// A system notice (connection, resume, errors, command output).
    Notice,
    /// A blank separator line between blocks.
    Separator,
}

/// One line of transcript, ready to be styled and written to scrollback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    /// Style family.
    pub kind: LineKind,
    /// Line text, with no trailing newline.
    pub text: String,
}

impl TranscriptLine {
    /// Build a line.
    pub fn new(kind: LineKind, text: impl Into<String>) -> Self {
        Self { kind, text: text.into() }
    }

    /// Build a blank separator line.
    pub fn separator() -> Self {
        Self::new(LineKind::Separator, "")
    }
}

/// True for a markdown code-fence marker line (```` ``` ````, optionally with a
/// language tag).
fn is_fence_marker(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// One in-flight assistant stream, fed by `chat.delta` and closed by
/// `chat.final`.
///
/// Lines become eligible for scrollback as soon as they are newline-terminated
/// — that is what makes the output appear to stream into the terminal rather
/// than landing in one dump at the end. Two things hold a line back:
///
/// - the last, still-growing line (it has no newline yet), and
/// - a fenced code block, which is held until its closing fence so the whole
///   block is styled as one unit. `HOLD_CAP` bounds that hold, so a pit of
///   unterminated code cannot starve the scrollback forever.
#[derive(Debug)]
pub struct StreamBuffer {
    /// How the streamed prose is tagged (assistant text or reasoning).
    kind: LineKind,
    /// Everything received, including what has already been emitted. Kept for
    /// the `chat.final` prefix comparison.
    full: String,
    /// Bytes of `full` already handed to the caller.
    emitted: usize,
    /// Complete lines that are newline-terminated but not yet emitted (the
    /// held code block).
    held: Vec<TranscriptLine>,
    /// Inside an unterminated fenced code block.
    in_fence: bool,
}

impl Default for StreamBuffer {
    fn default() -> Self {
        Self {
            kind: LineKind::Assistant,
            full: String::new(),
            emitted: 0,
            held: Vec::new(),
            in_fence: false,
        }
    }
}

/// How many lines a fenced block may hold before the oldest are released.
const HOLD_CAP: usize = 40;

impl StreamBuffer {
    /// A stream whose prose is tagged `kind`.
    pub fn new(kind: LineKind) -> Self {
        Self { kind, ..Self::default() }
    }

    /// Append a streamed delta.
    pub fn push_delta(&mut self, delta: &str) {
        self.full.push_str(delta);
    }

    /// True when nothing has been received yet.
    pub fn is_empty(&self) -> bool {
        self.full.is_empty()
    }

    /// True while a fenced block is being held back.
    pub fn in_fence(&self) -> bool {
        self.in_fence
    }

    /// The bytes of the stream not yet emitted — the held block plus the
    /// still-growing final line. Used for the live preview.
    pub fn pending(&self) -> &str {
        &self.full[self.emitted..]
    }

    /// Lines that may graduate into scrollback now.
    ///
    /// Consumes them: a returned line is never returned again.
    pub fn take_flushable(&mut self) -> Vec<TranscriptLine> {
        let mut out = Vec::new();
        // Complete lines only — the final partial line stays pending.
        while let Some(nl) = self.full[self.emitted..].find('\n') {
            let end = self.emitted + nl;
            let line = self.full[self.emitted..end]
                .trim_end_matches('\r')
                .to_string();
            self.emitted = end + 1;

            if self.in_fence {
                let closes = is_fence_marker(&line);
                self.held.push(TranscriptLine::new(LineKind::Code, line));
                if closes {
                    self.in_fence = false;
                    out.append(&mut self.held);
                } else if self.held.len() > HOLD_CAP {
                    // Bounded preview: release the oldest held lines so a long
                    // or unterminated block cannot hold the transcript.
                    let excess = self.held.len() - HOLD_CAP;
                    out.extend(self.held.drain(..excess));
                }
            } else if is_fence_marker(&line) {
                self.in_fence = true;
                self.held.push(TranscriptLine::new(LineKind::Code, line));
            } else {
                out.push(TranscriptLine::new(self.kind, line));
            }
        }
        out
    }

    /// Close the stream with the authoritative final text, returning every
    /// line still owed to scrollback.
    ///
    /// `chat.final` carries the whole turn's text, and for providers that do
    /// not stream, it is the *only* thing that arrives — so this must work
    /// with an empty accumulator as well as with one that already produced
    /// deltas.
    pub fn finish(&mut self, final_text: Option<&str>) -> Vec<TranscriptLine> {
        let mut out = self.take_flushable();
        let already_emitted = self.full[..self.emitted].to_string();

        match final_text {
            None => {}
            Some(final_text) => {
                let confirmed = final_text.starts_with(&already_emitted);
                if !confirmed && !already_emitted.is_empty() {
                    // The provider re-stated the response differently after we
                    // already printed part of it. Say so instead of silently
                    // dropping the authoritative text.
                    out.push(TranscriptLine::new(
                        LineKind::Notice,
                        "⚠ response was re-stated by the provider; the text above may differ",
                    ));
                }
                // Emit whatever of the final text we have not printed yet.
                let tail = final_text
                    .strip_prefix(&already_emitted)
                    .unwrap_or(final_text);
                if !tail.is_empty() {
                    out.extend(lines_of(tail, self.kind));
                }
            }
        }

        // Anything still pending (a trailing partial line, a held block).
        out.extend(std::mem::take(&mut self.held));
        let pending = self.pending().to_string();
        if !pending.is_empty() && final_text.is_none() {
            out.extend(lines_of(&pending, self.kind));
        }
        self.emitted = self.full.len();
        out
    }
}

/// Split text into styled transcript lines, tagging fenced regions as code.
fn lines_of(text: &str, kind: LineKind) -> Vec<TranscriptLine> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() && raw.is_empty() && out.is_empty() {
            continue;
        }
        if is_fence_marker(line) {
            in_fence = !in_fence;
            out.push(TranscriptLine::new(LineKind::Code, line));
        } else if in_fence {
            out.push(TranscriptLine::new(LineKind::Code, line));
        } else {
            out.push(TranscriptLine::new(kind, line));
        }
    }
    out
}

/// The transcript: lines waiting to be frozen, plus the in-flight streams.
#[derive(Debug, Default)]
pub struct Transcript {
    /// Complete lines not yet handed to the scrollback writer.
    ready: Vec<TranscriptLine>,
    /// In-flight assistant streams, keyed by message id, in arrival order.
    streams: Vec<(String, StreamBuffer)>,
}

impl Transcript {
    /// New, empty transcript.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a complete block of lines (a user echo, a notice, command output).
    pub fn push(&mut self, lines: impl IntoIterator<Item = TranscriptLine>) {
        self.ready.extend(lines);
    }

    /// Queue a single user-message echo.
    pub fn push_user(&mut self, text: &str) {
        self.ready
            .push(TranscriptLine::new(LineKind::User, format!("> {}", text)));
    }

    /// Queue a single notice line.
    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.ready.push(TranscriptLine::new(LineKind::Notice, text));
    }

    /// Queue a blank separator.
    pub fn push_separator(&mut self) {
        self.ready.push(TranscriptLine::separator());
    }

    /// Feed a streamed delta for `id`, creating its buffer on first sight.
    ///
    /// `kind` only applies when the stream is created; the buffer keeps the
    /// tag it started with.
    pub fn push_delta(&mut self, id: &str, kind: LineKind, delta: &str) {
        self.stream_mut(id, kind).push_delta(delta);
    }

    /// Close the stream for `id` with the authoritative final text.
    pub fn finish_stream(&mut self, id: &str, final_text: Option<&str>) {
        if let Some(idx) = self.streams.iter().position(|(sid, _)| sid == id) {
            let (_, mut buf) = self.streams.remove(idx);
            let mut lines = buf.finish(final_text);
            // Every completed turn is followed by a blank line, whether or not
            // this particular close produced any text of its own.
            lines.push(TranscriptLine::separator());
            self.ready.extend(lines);
        } else if let Some(text) = final_text {
            // No deltas ever arrived (non-streaming provider): the final text
            // is the whole response.
            let mut lines = lines_of(text, LineKind::Assistant);
            lines.push(TranscriptLine::separator());
            self.ready.extend(lines);
        }
    }

    /// Take everything that has graduated, for one scrollback write.
    pub fn take_flushable(&mut self) -> Vec<TranscriptLine> {
        let mut out = std::mem::take(&mut self.ready);
        for (_, buf) in self.streams.iter_mut() {
            out.extend(buf.take_flushable());
        }
        out
    }

    /// The tail of what is still live, newest last, at most `max` lines.
    pub fn preview(&self, max: usize) -> Vec<TranscriptLine> {
        let mut out: Vec<TranscriptLine> = Vec::new();
        for (_, buf) in self.streams.iter() {
            if buf.is_empty() {
                continue;
            }
            let mut lines: Vec<TranscriptLine> = buf.held.clone();
            lines.extend(lines_of(buf.pending(), buf.kind));
            out.extend(lines);
        }
        if out.len() > max {
            out.split_off(out.len() - max)
        } else {
            out
        }
    }

    /// Close every open stream, keeping whatever text already arrived.
    ///
    /// For a turn that cannot be completed — an abort, a dropped connection.
    /// The text did arrive; it just never got an ending, and leaving the
    /// stream open would keep it in the live region forever.
    pub fn finish_open_streams(&mut self) {
        let open: Vec<String> = self.streams.iter().map(|(id, _)| id.clone()).collect();
        for id in open {
            self.finish_stream(&id, None);
        }
    }

    /// Drop in-flight streams (session switch, `/clear`).
    ///
    /// Scrollback is the terminal's and is deliberately untouched.
    pub fn reset(&mut self) {
        self.ready.clear();
        self.streams.clear();
    }

    fn stream_mut(&mut self, id: &str, kind: LineKind) -> &mut StreamBuffer {
        if let Some(idx) = self.streams.iter().position(|(sid, _)| sid == id) {
            return &mut self.streams[idx].1;
        }
        self.streams.push((id.to_string(), StreamBuffer::new(kind)));
        let last = self.streams.len() - 1;
        &mut self.streams[last].1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[TranscriptLine]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn holds_the_partial_line_until_its_newline_arrives() {
        let mut buf = StreamBuffer::default();
        buf.push_delta("hello");
        assert!(buf.take_flushable().is_empty(), "no newline yet");
        buf.push_delta(" world\n");
        assert_eq!(texts(&buf.take_flushable()), vec!["hello world"]);
        assert!(buf.take_flushable().is_empty(), "emitted once, not twice");
    }

    #[test]
    fn splits_a_delta_that_spans_several_lines() {
        let mut buf = StreamBuffer::default();
        buf.push_delta("one\ntwo\nthree");
        assert_eq!(texts(&buf.take_flushable()), vec!["one", "two"]);
        assert_eq!(buf.pending(), "three");
    }

    /// Deltas can land mid-UTF-8-boundary in the middle of a multi-byte char;
    /// nothing may panic and nothing may be lost.
    #[test]
    fn survives_a_multibyte_delta_boundary() {
        let mut buf = StreamBuffer::default();
        buf.push_delta("中文");
        buf.push_delta("测试\n");
        assert_eq!(texts(&buf.take_flushable()), vec!["中文测试"]);
    }

    /// The path the UI actually takes: every delta immediately followed by a
    /// flush attempt. Nothing may be dropped or duplicated.
    #[test]
    fn one_char_at_a_time_loses_nothing() {
        let text = "a\nbb\nccc\n";
        let mut buf = StreamBuffer::default();
        let mut emitted: Vec<String> = Vec::new();
        for ch in text.chars() {
            buf.push_delta(&ch.to_string());
            emitted.extend(buf.take_flushable().into_iter().map(|l| l.text));
        }
        emitted.extend(buf.finish(Some(text)).into_iter().map(|l| l.text));
        assert_eq!(emitted, vec!["a", "bb", "ccc"]);
        assert_eq!(buf.pending(), "");
    }

    #[test]
    fn holds_a_fenced_block_until_it_closes() {
        let mut buf = StreamBuffer::default();
        buf.push_delta("before\n```rust\nlet x = 1;\n");
        // The fence and everything after it stay live.
        assert_eq!(texts(&buf.take_flushable()), vec!["before"]);
        assert!(buf.in_fence());
        buf.push_delta("```\nafter\n");
        let flushed = buf.take_flushable();
        assert_eq!(texts(&flushed), vec!["```rust", "let x = 1;", "```", "after"]);
        assert!(flushed[..3].iter().all(|l| l.kind == LineKind::Code));
        assert_eq!(flushed[3].kind, LineKind::Assistant);
    }

    #[test]
    fn releases_the_oldest_lines_of_an_overlong_fence() {
        let mut buf = StreamBuffer::default();
        buf.push_delta("```\n");
        let body = HOLD_CAP + 5;
        for i in 0..body {
            buf.push_delta(&format!("line {i}\n"));
        }
        let flushed = buf.take_flushable();
        // Everything past the hold cap is released, oldest first — the fence
        // never holds more than HOLD_CAP lines, and never loses any.
        assert_eq!(flushed.len(), 1 + body - HOLD_CAP);
        assert_eq!(flushed[0].text, "```");
        assert_eq!(flushed[1].text, "line 0");
        assert!(buf.in_fence(), "still inside the fence");
        assert_eq!(buf.held.len(), HOLD_CAP);
    }

    #[test]
    fn final_flushes_the_rest_of_a_streamed_turn() {
        let mut buf = StreamBuffer::default();
        buf.push_delta("kept\nstill growing");
        assert_eq!(texts(&buf.take_flushable()), vec!["kept"]);
        let out = buf.finish(Some("kept\nstill growing, and done\n"));
        assert_eq!(texts(&out), vec!["still growing, and done", ""]);
    }

    #[test]
    fn final_alone_covers_a_non_streaming_provider() {
        let mut buf = StreamBuffer::default();
        let out = buf.finish(Some("whole response\nsecond line\n"));
        assert_eq!(texts(&out), vec!["whole response", "second line", ""]);
    }

    /// A provider that re-states the response differently must not lose the
    /// authoritative text, and must say that the printed text may differ.
    #[test]
    fn final_notices_a_divergent_restatement() {
        let mut buf = StreamBuffer::default();
        buf.push_delta("original line\n");
        buf.take_flushable();
        let out = buf.finish(Some("different line\nextra\n"));
        assert_eq!(out[0].kind, LineKind::Notice);
        assert!(out[0].text.contains("re-stated"));
        assert_eq!(texts(&out[1..]), vec!["different line", "extra", ""]);
    }

    #[test]
    fn final_without_text_flushes_what_is_pending() {
        let mut buf = StreamBuffer::default();
        buf.push_delta("tail without newline");
        let out = buf.finish(None);
        assert_eq!(texts(&out), vec!["tail without newline"]);
    }

    #[test]
    fn transcript_queues_notices_and_user_lines_in_order() {
        let mut t = Transcript::new();
        t.push_user("hi");
        t.push_notice("connected");
        assert_eq!(texts(&t.take_flushable()), vec!["> hi", "connected"]);
        assert!(t.take_flushable().is_empty());
    }

    #[test]
    fn transcript_finishes_a_stream_with_a_separator() {
        let mut t = Transcript::new();
        t.push_delta("m1", LineKind::Assistant, "answer\n");
        assert_eq!(texts(&t.take_flushable()), vec!["answer"]);
        t.finish_stream("m1", Some("answer\n"));
        assert_eq!(texts(&t.take_flushable()), vec![""]);
    }

    /// The preview shows only what has not graduated, newest last, capped.
    #[test]
    fn preview_is_the_live_tail_only() {
        let mut t = Transcript::new();
        t.push_delta("m1", LineKind::Assistant, "frozen\n");
        t.take_flushable();
        t.push_delta("m1", LineKind::Assistant, "live one\nlive two");
        assert_eq!(texts(&t.preview(10)), vec!["live one", "live two"]);
        assert_eq!(texts(&t.preview(1)), vec!["live two"]);
    }

    /// Closing an open stream keeps what arrived and ends it, so nothing is
    /// left sitting in the live region.
    #[test]
    fn finish_open_streams_flushes_what_arrived() {
        let mut t = Transcript::new();
        t.push_delta("assistant", LineKind::Assistant, "half a line\nand a tail");
        t.push_delta("thinking", LineKind::Reasoning, "still thinking");
        t.finish_open_streams();

        let flushed: Vec<String> = t.take_flushable().into_iter().map(|l| l.text).collect();
        assert!(flushed.contains(&"half a line".to_string()));
        assert!(flushed.contains(&"and a tail".to_string()));
        assert!(flushed.contains(&"still thinking".to_string()));
        assert!(t.preview(10).is_empty(), "nothing is left live");
    }

    #[test]
    fn reset_drops_live_state() {
        let mut t = Transcript::new();
        t.push_delta("m1", LineKind::Assistant, "half a line");
        t.push_notice("queued");
        t.reset();
        assert!(t.take_flushable().is_empty());
        assert!(t.preview(10).is_empty());
    }
}
