//! Per-service log buffer.
//!
//! Container output is fed straight into a `vt100` terminal emulator, the same
//! way nx feeds its PTY output. That gets us SGR colour, `\r` progress rewrites
//! and cursor motion handled correctly, plus per-cell access so search matches
//! can be highlighted over already-coloured output.
//!
//! Scrollback semantics follow nx: the offset counts rows *back from the
//! bottom*, so `0` means "tailing live output".

/// Rows of scrollback retained per service. Matches nx's `SCROLLBACK_SIZE`.
pub const DEFAULT_SCROLLBACK: usize = 1_000;

/// Hard ceiling on the retained buffer, per service.
///
/// The line budget below is what keeps the buffer correct; this only guards
/// against pathological content — enormous lines, or output with no newlines at
/// all — where a line count says nothing about size. Hitting it can still cost
/// retained history, but at eight megabytes rather than at a few hundred
/// kilobytes, which is where a bytes-per-row estimate used to give out.
const MAX_RAW_BYTES: usize = 8 * 1024 * 1024;

/// The SGR sequence reproducing the styling left active after `prefix` and then
/// `dropped` are parsed.
///
/// Both halves defer to the crate. The dropped bytes go through a scratch
/// emulator rather than being scanned for escape codes, and the result is
/// serialised by `attributes_formatted`, which diffs the parser's own attribute
/// state against the default. Hand-enumerating attributes is what dropped `dim`
/// once already, and would drop the next one the crate learns about.
fn pen_after(prefix: &[u8], dropped: &[u8]) -> Vec<u8> {
    let mut scratch = vt100::Parser::new(MIN_ROWS, MIN_COLS, 0);
    scratch.process(prefix);
    scratch.process(dropped);
    scratch.screen().attributes_formatted()
}

fn bytecount(bytes: &[u8]) -> usize {
    bytes.iter().filter(|b| **b == b'\n').count()
}

fn keep_lines_for(scrollback: usize, rows: u16) -> usize {
    scrollback.saturating_add(rows as usize).max(1)
}

/// Floor on the emulated screen size.
///
/// `vt100` underflows in `col_wrap` on very narrow grids, so this is a crash
/// guard rather than a cosmetic minimum. It matches the floor the pane's own
/// geometry already applies, so it never binds in the render path.
const MIN_ROWS: u16 = 3;
const MIN_COLS: u16 = 20;

/// Size used before the first layout pass tells us the real pane geometry.
const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;

pub struct LogStore {
    parser: vt100::Parser,
    /// Rows of scrollback the parser retains; needed to rebuild it on resize.
    scrollback_len: usize,
    /// Normalised bytes as fed to the parser, replayed when the width changes.
    ///
    /// `vt100` stores rows already wrapped and does not reflow them, so the
    /// only way to rewrap history is to parse it again at the new width.
    raw: Vec<u8>,
    /// Complete lines to keep in `raw`.
    keep_lines: usize,
    /// Newlines currently in `raw`, tracked so trimming does not rescan.
    lines: usize,
    /// Whether the previous chunk ended on a carriage return, so a `\r\n` split
    /// across chunks isn't mistaken for a bare newline.
    pending_cr: bool,
    /// Set when a replay had to be skipped, so the next resize retries instead
    /// of short-circuiting on a size that was applied without one.
    replay_pending: bool,
    /// The styling active where `raw` begins.
    ///
    /// Trimming drops the bytes that set it, so without this a replay would
    /// render the retained lines in default colours. A service that sets a
    /// colour once and leaves it on loses it otherwise.
    pen: Vec<u8>,
    /// True once any output at all has been received.
    has_output: bool,
}

impl LogStore {
    pub fn new(scrollback: usize) -> Self {
        Self {
            parser: vt100::Parser::new(INITIAL_ROWS, INITIAL_COLS, scrollback),
            scrollback_len: scrollback,
            raw: Vec::new(),
            keep_lines: keep_lines_for(scrollback, INITIAL_ROWS),
            lines: 0,
            pending_cr: false,
            pen: Vec::new(),
            replay_pending: false,
            has_output: false,
        }
    }

    pub fn has_output(&self) -> bool {
        self.has_output
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Feeds raw container output to the emulator.
    ///
    /// A view at offset 0 keeps tailing; a view scrolled up stays on the content
    /// it is showing. Both come from `vt100`, which advances the scroll offset
    /// as rows evict into scrollback, so nothing here needs to compensate.
    ///
    /// Doing that by hand was worse than doing nothing. The correction only had
    /// a signal to work from while the buffer was filling, and once it was full
    /// it overwrote the emulator's own -- correct -- offset, producing exactly
    /// the drift it was meant to prevent.
    pub fn process(&mut self, bytes: &[u8]) {
        // An empty write is not output: marking it as such would replace the
        // "waiting" placeholder with a blank pane.
        if bytes.is_empty() {
            return;
        }
        let normalised = self.normalise_newlines(bytes);
        self.retain(&normalised);
        self.parser.process(&normalised);
        self.has_output = true;
    }

    /// Turns a lone `\n` into `\r\n`, the way a terminal driver's ONLCR would.
    ///
    /// Container logs are LF-terminated. Fed to the emulator raw, a bare `\n`
    /// moves the cursor down without returning it to column 0, so every line
    /// starts where the last one ended and the output walks off to the right.
    ///
    /// The carry flag matters: output arrives in arbitrary chunks, so a `\r` can
    /// end one and its `\n` begin the next. Deciding per chunk would insert a
    /// spurious `\r` at that seam.
    fn normalise_newlines(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        let mut prev_cr = self.pending_cr;
        for &byte in input {
            if byte == b'\n' && !prev_cr {
                out.push(b'\r');
            }
            out.push(byte);
            prev_cr = byte == b'\r';
        }
        self.pending_cr = prev_cr;
        out
    }

    /// Keeps the output needed to rewrap on resize.
    ///
    /// Trimming scans the buffer, so it runs once the surplus is worth the
    /// scan rather than on every write.
    fn retain(&mut self, bytes: &[u8]) {
        self.raw.extend_from_slice(bytes);
        self.lines += bytecount(bytes);

        let comfortable = self.lines <= self.keep_lines.saturating_mul(2);
        if comfortable && self.raw.len() <= MAX_RAW_BYTES {
            return;
        }

        let cut = self.trim_point();
        if cut == 0 {
            return;
        }
        self.pen = pen_after(&self.pen, &self.raw[..cut]);
        self.lines -= bytecount(&self.raw[..cut]);
        // `drain` would leave the old capacity behind: one oversized chunk can
        // hold many times the ceiling for the life of the store. `split_off`
        // hands back a right-sized buffer and frees the original.
        let tail = self.raw.split_off(cut);
        self.raw = tail;
    }

    /// Where to cut so the newest `keep_lines` lines survive, without letting
    /// the buffer past its ceiling.
    fn trim_point(&self) -> usize {
        let mut seen = 0;
        let mut cut = 0;
        for (index, byte) in self.raw.iter().enumerate().rev() {
            if *byte == b'\n' {
                seen += 1;
                if seen > self.keep_lines {
                    cut = index + 1;
                    break;
                }
            }
        }

        if self.raw.len() - cut <= MAX_RAW_BYTES {
            return cut;
        }

        // Over the ceiling even after keeping only the budgeted lines, so the
        // lines themselves are enormous or there are none at all. Cut to the
        // ceiling, preferring a line boundary; landing mid-sequence garbles at
        // most the first replayed row, which beats losing the pane.
        let floor = self.raw.len() - MAX_RAW_BYTES;
        self.raw[floor..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|offset| floor + offset + 1)
            // A line boundary at the very end would cut everything away, which
            // is the one outcome worse than a garbled first row.
            .filter(|cut| *cut < self.raw.len())
            .unwrap_or(floor)
    }

    /// Resizes the emulated terminal to the pane's inner area.
    ///
    /// Any size change rebuilds the parser and replays the retained bytes.
    ///
    /// A width change needs it because `vt100` keeps rows at the width they
    /// arrived at and will not rewrap them. A height change needs it because
    /// `Grid::set_size` shrinks its row vector from the end, discarding the
    /// newest lines rather than moving them into scrollback -- which left a
    /// pane unable to reach its own tail, with `End` powerless because the
    /// offset was already at the bottom of what remained.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(MIN_ROWS);
        let cols = cols.max(MIN_COLS);
        let (cur_rows, cur_cols) = self.parser.screen().size();

        // A pending replay has to defeat this, or a resize back to a size that
        // was applied without one would short-circuit and never replay at all.
        if (cur_rows, cur_cols) == (rows, cols) && !self.replay_pending {
            return;
        }

        // More visible rows means more history to be able to reproduce.
        self.keep_lines = keep_lines_for(self.scrollback_len, rows);

        let old_offset = self.parser.screen().scrollback();

        if self.raw.is_empty() && self.has_output {
            // Rebuilding from nothing would blank a pane that has content.
            // Keep what is on screen and try again on the next resize.
            self.parser.screen_mut().set_size(rows, cols);
            self.replay_pending = true;
        } else {
            let mut rebuilt = vt100::Parser::new(rows, cols, self.scrollback_len);
            rebuilt.process(&self.pen);
            rebuilt.process(&self.raw);
            self.parser = rebuilt;
            self.replay_pending = false;
        }

        // Losing height moves the bottom of the window up under a scrolled-up
        // reader, so pull the offset back by the rows lost. Anything else keeps
        // its position.
        let target = if rows < cur_rows && old_offset > 0 {
            old_offset.saturating_sub((cur_rows - rows) as usize)
        } else {
            old_offset
        };
        self.parser.screen_mut().set_scrollback(target);
    }

    /// Rows scrolled back from the bottom. `0` means tailing.
    pub fn scroll_offset(&self) -> usize {
        self.parser.screen().scrollback()
    }

    pub fn scroll_up(&mut self, lines: u16) {
        let target = self.scroll_offset().saturating_add(lines as usize);
        self.parser.screen_mut().set_scrollback(target);
    }

    pub fn scroll_down(&mut self, lines: u16) {
        let target = self.scroll_offset().saturating_sub(lines as usize);
        self.parser.screen_mut().set_scrollback(target);
    }

    pub fn scroll_to_top(&mut self) {
        // vt100 clamps to the number of retained rows.
        self.parser.screen_mut().set_scrollback(usize::MAX);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }

    /// The full retained buffer as plain text, for clipboard copy.
    ///
    /// Walks the scrollback from the top down. Each window overlaps the next by
    /// all but `advance` rows, so only the first `advance` rows of each window
    /// are new — taking the whole window would duplicate content whenever the
    /// scrollback depth is not an exact multiple of the pane height.
    pub fn all_text(&mut self) -> String {
        let saved = self.scroll_offset();
        let (rows, cols) = self.parser.screen().size();

        self.parser.screen_mut().set_scrollback(usize::MAX);
        let mut offset = self.parser.screen().scrollback();

        let mut lines: Vec<String> = Vec::new();
        loop {
            self.parser.screen_mut().set_scrollback(offset);
            let window = self.parser.screen().rows(0, cols);
            if offset == 0 {
                lines.extend(window);
                break;
            }
            let advance = (rows as usize).min(offset);
            lines.extend(window.take(advance));
            offset -= advance;
        }

        self.parser.screen_mut().set_scrollback(saved);

        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        let mut out = lines.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }

    /// Visible rows as plain strings.
    ///
    /// Test-only: rendering blits cells straight from the emulator, and the
    /// scrollbar measures the screen's geometry, so nothing in the running
    /// program needs the text materialised.
    #[cfg(test)]
    pub fn visible_lines(&self) -> Vec<String> {
        let (_, cols) = self.parser.screen().size();
        self.parser.screen().rows(0, cols).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The view is tailing when it sits at the bottom of the buffer.
    fn tailing(store: &LogStore) -> bool {
        store.scroll_offset() == 0
    }

    /// Visible rows with the blank padding removed.
    fn non_empty(store: &LogStore) -> Vec<String> {
        store
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }

    fn store_with(lines: usize) -> LogStore {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        for i in 0..lines {
            s.process(format!("line {i}\r\n").as_bytes());
        }
        s
    }

    /// The stream layer bounds how much one write carries, so output that
    /// used to arrive in a single write can now arrive cut at an offset with
    /// no relation to its content: mid escape sequence, between a `\r` and its
    /// `\n`, inside a multi-byte character. That is only safe because the
    /// emulator's parse state survives between writes, which is verified here
    /// rather than assumed. The `pending_cr` carry needs its own test: a
    /// duplicated `\r` is invisible to a terminal, so a broken carry would
    /// slip past this comparison.
    ///
    /// Cutting every seven bytes is the harsher version of a 64 KiB cut: it
    /// puts a seam inside every construct in the input instead of one seam
    /// somewhere in it.
    #[test]
    fn output_renders_the_same_however_it_was_cut_up() {
        let mut whole = LogStore::new(DEFAULT_SCROLLBACK);
        whole.resize(10, 40);
        let mut split = LogStore::new(DEFAULT_SCROLLBACK);
        split.resize(10, 40);

        let mut input = Vec::new();
        for i in 0..200 {
            let colour = 31 + i % 7;
            input.extend_from_slice(format!("\x1b[{colour}mrow {i} k\u{e9}\r\n").as_bytes());
        }

        whole.process(&input);
        for piece in input.chunks(7) {
            split.process(piece);
        }

        // Formatted contents, not plain rows: a cut inside an SGR sequence
        // loses the colour rather than the text, which plain rows would miss.
        assert_eq!(
            split.screen().contents_formatted(),
            whole.screen().contents_formatted(),
            "the pane renders differently when the output arrives cut up"
        );
        assert_eq!(split.all_text(), whole.all_text());
    }

    /// The `pending_cr` carry, which the comparison above cannot see: a `\r`
    /// inserted twice moves the cursor to column zero twice, so a broken carry
    /// is invisible in the rendered output of CRLF input. What it is not
    /// invisible in is *bare* LF input, where a carry wrongly reported as set
    /// suppresses the `\r` this normalisation exists to insert and the line
    /// walks off to the right.
    ///
    /// Splitting a frame is what makes this reachable: container logs are
    /// LF-terminated, so a cut at a piece boundary routinely leaves a bare
    /// `\n` starting the next write.
    #[test]
    fn a_bare_newline_starting_a_write_still_returns_to_column_zero() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(6, 40);

        // The cut lands between the line and its terminator, so the second
        // write opens on the `\n` with no `\r` anywhere near it.
        s.process(b"first line");
        s.process(b"\nsecond line\n");

        assert_eq!(non_empty(&s), ["first line", "second line"]);
    }

    #[test]
    fn an_empty_write_is_not_output() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(5, 40);
        s.process(b"");
        assert!(
            !s.has_output(),
            "an empty write should leave the pane waiting"
        );
    }

    #[test]
    fn a_new_store_reports_no_output() {
        let s = LogStore::new(DEFAULT_SCROLLBACK);
        assert!(!s.has_output());
    }

    #[test]
    fn processing_marks_output_and_tails() {
        let s = store_with(3);
        assert!(s.has_output());
        assert!(tailing(&s), "a fresh store should be pinned to the bottom");
    }

    #[test]
    fn newest_lines_are_visible_when_tailing() {
        let s = store_with(50);
        let visible = s.visible_lines();
        assert!(
            visible.iter().any(|l| l.contains("line 49")),
            "expected the newest line, got: {visible:?}"
        );
        assert!(!visible.iter().any(|l| l.contains("line 0")));
    }

    #[test]
    fn scrolling_up_then_down_returns_to_tailing() {
        let mut s = store_with(50);
        s.scroll_up(5);
        assert_eq!(s.scroll_offset(), 5);
        assert!(!tailing(&s));
        s.scroll_down(5);
        assert!(tailing(&s));
    }

    #[test]
    fn scrolling_down_past_the_bottom_clamps() {
        let mut s = store_with(50);
        s.scroll_up(3);
        s.scroll_down(999);
        assert_eq!(s.scroll_offset(), 0);
    }

    #[test]
    fn scrolling_up_past_the_top_clamps_to_retained_rows() {
        let mut s = store_with(50);
        s.scroll_up(u16::MAX);
        let max = s.scroll_offset();
        assert!(max > 0 && max < 50, "expected a bounded top, got {max}");
        // Already at the top: going further changes nothing.
        s.scroll_up(10);
        assert_eq!(s.scroll_offset(), max);
    }

    #[test]
    fn top_and_bottom_helpers_reach_the_extremes() {
        let mut s = store_with(50);
        s.scroll_to_top();
        assert!(!tailing(&s));
        s.scroll_to_bottom();
        assert!(tailing(&s));
    }

    #[test]
    fn a_tailing_view_keeps_tailing_as_output_arrives() {
        let mut s = store_with(20);
        s.process(b"newest\r\n");
        assert!(tailing(&s));
        assert!(s.visible_lines().iter().any(|l| l.contains("newest")));
    }

    #[test]
    fn a_scrolled_view_stays_on_the_same_content_as_output_arrives() {
        let mut s = store_with(20);
        s.scroll_up(4);
        let before: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();

        s.process(b"newest\r\n");

        let after: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        assert_eq!(
            before, after,
            "a scrolled-up reader should not be dragged along by new output"
        );
        assert_eq!(s.scroll_offset(), 5, "the offset absorbs the new row");
    }

    #[test]
    fn a_scrolled_view_holds_still_even_once_the_buffer_is_full() {
        // Regression: hand-rolled anchoring used to overwrite the emulator's own
        // offset here, which drifted the view and was mistaken for a limitation
        // of the emulator.
        let mut s = LogStore::new(64);
        s.resize(10, 40);
        for i in 0..500 {
            s.process(format!("line {i}\n").as_bytes());
        }
        s.scroll_up(5);
        let before = non_empty(&s);
        for i in 0..20 {
            s.process(format!("burst {i}\n").as_bytes());
        }
        assert_eq!(
            before,
            non_empty(&s),
            "a scrolled-up reader should not be dragged along by new output"
        );
    }

    #[test]
    fn a_scrolled_view_absorbs_a_burst_of_output() {
        let mut s = store_with(30);
        s.scroll_up(6);
        let before: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        for i in 0..25 {
            s.process(format!("burst {i}\r\n").as_bytes());
        }
        let after: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        assert_eq!(before, after, "content should hold still through a burst");
    }

    // ---- newline normalisation (#11) ----

    #[test]
    fn bare_line_feeds_start_at_column_zero() {
        // Container logs are LF-terminated. Fed raw to the emulator, each line
        // would start where the last one ended and walk off to the right.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(6, 40);
        s.process(b"first line\nsecond line\nthird line\n");
        let lines: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines, ["first line", "second line", "third line"]);
    }

    #[test]
    fn carriage_return_line_feed_is_left_alone() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(6, 40);
        s.process(b"alpha\r\nbeta\r\n");
        let lines: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines, ["alpha", "beta"]);
    }

    #[test]
    fn a_crlf_split_across_chunks_is_not_treated_as_a_bare_newline() {
        // The seam case: deciding per chunk would insert a spurious \r here.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(6, 40);
        s.process(b"alpha\r");
        s.process(b"\nbeta\r\n");
        let lines: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines, ["alpha", "beta"]);
    }

    #[test]
    fn a_line_split_mid_word_across_chunks_still_reads_correctly() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(6, 40);
        s.process(b"data");
        s.process(b"base ready\n");
        assert!(s
            .visible_lines()
            .iter()
            .any(|l| l.trim_end() == "database ready"));
    }

    // ---- reflow on resize (#8) ----

    #[test]
    fn widening_rewraps_existing_output() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        let long = "A".repeat(30) + &"B".repeat(30);
        s.process(format!("{long}\n").as_bytes());
        // At 40 columns it needs two rows.
        assert_eq!(non_empty(&s).len(), 2);

        s.resize(10, 100);
        // At 100 it fits on one, which only happens if history was reparsed.
        let after = non_empty(&s);
        assert_eq!(after.len(), 1, "history should rewrap, got {after:?}");
        assert_eq!(after[0], long);
    }

    #[test]
    fn narrowing_rewraps_existing_output() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 100);
        let long = "A".repeat(30) + &"B".repeat(30);
        s.process(format!("{long}\n").as_bytes());
        assert_eq!(non_empty(&s).len(), 1);

        s.resize(10, 40);
        assert_eq!(non_empty(&s).len(), 2, "narrowing should rewrap too");
    }

    #[test]
    fn shrinking_height_keeps_the_newest_lines_reachable() {
        // vt100's Grid::set_size shrinks its row vector from the end, so the
        // newest lines were discarded outright. The pane could not reach its own
        // tail, and End was powerless because the offset was already 0.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(24, 100);
        for i in 0..400 {
            s.process(format!("L{i:03}\n").as_bytes());
        }
        assert_eq!(non_empty(&s).last().map(String::as_str), Some("L399"));

        s.resize(12, 100);
        s.scroll_to_bottom();
        assert_eq!(
            non_empty(&s).last().map(String::as_str),
            Some("L399"),
            "the tail must survive a height reduction"
        );
    }

    #[test]
    fn growing_height_reveals_more_without_losing_the_tail() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 100);
        for i in 0..400 {
            s.process(format!("L{i:03}\n").as_bytes());
        }
        s.resize(30, 100);
        let visible = non_empty(&s);
        assert_eq!(visible.last().map(String::as_str), Some("L399"));
        assert!(visible.len() > 10, "a taller pane should show more rows");
    }

    #[test]
    fn a_height_only_change_keeps_the_content() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        s.process(b"stable line\n");
        let before = non_empty(&s);
        s.resize(20, 40);
        assert_eq!(before, non_empty(&s));
    }

    #[test]
    fn scroll_position_survives_a_widen() {
        let mut s = store_with(60);
        s.scroll_up(5);
        s.resize(10, 100);
        assert_eq!(s.scroll_offset(), 5, "widening should not move the reader");
    }

    #[test]
    fn losing_height_pulls_a_scrolled_reader_back_by_the_rows_lost() {
        let mut s = store_with(60);
        s.scroll_up(10);
        s.resize(6, 40); // same width, four rows shorter
        assert_eq!(s.scroll_offset(), 6);
    }

    #[test]
    fn a_tailing_reader_keeps_tailing_across_a_resize() {
        let mut s = store_with(60);
        assert!(tailing(&s));
        s.resize(10, 100);
        assert!(tailing(&s), "a reader at the bottom should stay there");
    }

    #[test]
    fn the_retained_buffer_is_bounded_and_cut_at_a_line_boundary() {
        let mut s = LogStore::new(16);
        s.resize(5, 40);
        for i in 0..5_000 {
            s.process(format!("line {i} with some padding to take up room\n").as_bytes());
        }
        assert!(
            s.lines <= s.keep_lines * 2,
            "retained lines must stay bounded"
        );
        assert!(
            s.raw.starts_with(b"line "),
            "a trim must land on a line boundary, not mid-sequence"
        );
        // And it still rewraps correctly after trimming.
        s.resize(5, 100);
        assert!(non_empty(&s).iter().any(|l| l.contains("line 4999")));
    }

    #[test]
    fn output_without_newlines_survives_a_resize() {
        // A \r-driven progress bar emits no newline at all, so the retained
        // buffer can pass its cap with no line boundary to trim at.
        let mut s = LogStore::new(16);
        s.resize(5, 40);
        s.process(b"visible content\n");
        let blob = vec![b'X'; MAX_RAW_BYTES + 1];
        s.process(&blob);

        assert!(
            !s.raw.is_empty(),
            "an oversized record must not discard every retained byte"
        );
        s.resize(5, 100);
        assert!(
            !non_empty(&s).is_empty(),
            "the pane went blank after a resize"
        );
    }

    #[test]
    fn a_skipped_replay_is_retried_rather_than_lost() {
        // Applying a size without rewrapping used to make the early-return
        // treat that size as done, so the pane never rewrapped at that width
        // again -- silently reintroducing the bug this all exists to fix.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        let long = "A".repeat(30) + &"B".repeat(30);
        s.process(format!("{long}\n").as_bytes());

        s.raw.clear();
        s.resize(10, 100);
        assert!(s.replay_pending, "the replay should be recorded as owed");
        assert_eq!(non_empty(&s).len(), 2, "old wrapping is kept, not blanked");

        // Buffer refills; the same width must now actually rewrap.
        s.process(format!("{long}\n").as_bytes());
        s.resize(10, 100);
        assert!(!s.replay_pending);
        assert!(
            non_empty(&s).iter().any(|l| l == &long),
            "the retried rewrap should have unwrapped the line"
        );
    }

    #[test]
    fn a_resize_never_blanks_a_pane_that_has_content() {
        // Belt and braces for the case above: even if the retained buffer were
        // empty, what is already on screen must survive a width change.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(5, 40);
        s.process(b"still here\n");
        s.raw.clear();
        s.resize(5, 100);
        assert!(non_empty(&s).iter().any(|l| l.contains("still here")));
    }

    /// Fills past the retained cap so a trim is guaranteed, then rewraps.
    fn trim_then_resize(setup: &[u8]) -> LogStore {
        let mut s = LogStore::new(16);
        s.resize(5, 40);
        s.process(setup);
        let filler = "y".repeat(4096);
        for _ in 0..200 {
            s.process(format!("{filler}\n").as_bytes());
        }
        assert!(!s.pen.is_empty(), "a trim should have happened");
        s.resize(5, 100);
        s
    }

    #[test]
    fn styling_set_before_the_retained_window_survives_a_resize() {
        // The bytes that set the colour are trimmed away, so a replay would
        // render everything in the default colour without the carried pen.
        let s = trim_then_resize(b"\x1b[31m");
        assert_eq!(
            s.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
    }

    #[test]
    fn a_carried_pen_covers_attributes_and_backgrounds() {
        let s = trim_then_resize(b"\x1b[1m\x1b[4m\x1b[44m");
        let cell = s.screen().cell(0, 0).unwrap();
        assert!(cell.bold(), "bold should survive");
        assert!(cell.underline(), "underline should survive");
        assert_eq!(cell.bgcolor(), vt100::Color::Idx(4));
    }

    #[test]
    fn a_carried_pen_covers_dim() {
        let s = trim_then_resize(b"\x1b[2m");
        assert!(s.screen().cell(0, 0).unwrap().dim(), "dim should survive");
    }

    #[test]
    fn a_carried_pen_covers_bright_indexed_and_rgb_colours() {
        let bright = trim_then_resize(b"\x1b[91m");
        assert_eq!(
            bright.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Idx(9)
        );

        let indexed = trim_then_resize(b"\x1b[38;5;200m");
        assert_eq!(
            indexed.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Idx(200)
        );

        let rgb = trim_then_resize(b"\x1b[38;2;10;20;30m");
        assert_eq!(
            rgb.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Rgb(10, 20, 30)
        );
    }

    #[test]
    fn styling_that_was_reset_before_the_trim_is_not_resurrected() {
        // The pen must reflect the state at the trim boundary, not every
        // sequence that ever appeared.
        let s = trim_then_resize(b"\x1b[31m\x1b[0m");
        assert_eq!(
            s.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Default
        );
    }

    #[test]
    fn colour_dense_lines_are_not_lost_by_a_rewrap() {
        // Bytes per visual row depend on the content, not just the width: output
        // that reissues an SGR code per character costs many times a plain row.
        // A bytes-per-row budget silently dropped most of the scrollback here.
        let scrollback = 500;
        let mut s = LogStore::new(scrollback);
        s.resize(20, 80);
        for i in 0..scrollback {
            let mut line = format!("L{i:05} ");
            for c in 0..70 {
                line.push_str(&format!("\x1b[38;5;{}m", 16 + (c % 216)));
                line.push('x');
            }
            s.process(format!("{line}\n").as_bytes());
        }

        s.resize(20, 100);
        s.scroll_to_top();
        let after = non_empty(&s);
        assert!(
            after.iter().any(|l| l.trim_start().starts_with("L00000")),
            "the oldest retained line was lost by the rewrap: {:?}",
            after.first()
        );
    }

    #[test]
    fn a_wide_pane_keeps_its_oldest_row_across_a_width_change() {
        // The retention budget must scale with the pane: at ~1000 columns a
        // fixed per-row guess trimmed rows the parser was still holding, so a
        // rewrap silently dropped the top of the scrollback.
        let scrollback = 200;
        let mut s = LogStore::new(scrollback);
        s.resize(20, 1000);
        for i in 0..scrollback {
            // Rows that fill the width, which is the expensive case.
            s.process(format!("{i:04} {}\n", "w".repeat(980)).as_bytes());
        }

        s.scroll_to_top();
        let oldest = non_empty(&s)
            .first()
            .cloned()
            .expect("something should be on screen at the top");

        s.resize(20, 1100);
        s.scroll_to_top();
        let after = non_empty(&s);
        assert!(
            after.iter().any(|l| l.starts_with(&oldest[..8])),
            "oldest retained row was lost by the rewrap: {:?}",
            after.first()
        );
    }

    #[test]
    fn the_retention_budget_grows_with_the_pane() {
        let mut s = LogStore::new(1000);
        s.resize(20, 80);
        let shallow = s.keep_lines;
        s.resize(60, 1000);
        assert!(
            s.keep_lines > shallow,
            "more visible rows means more history to reproduce: {shallow} -> {}",
            s.keep_lines
        );
    }

    #[test]
    fn an_oversized_chunk_does_not_leave_its_allocation_behind() {
        // Trimming the length is not enough: the buffer would keep the peak
        // capacity for the life of the store.
        let mut s = LogStore::new(16);
        s.resize(5, 40);
        s.process(&vec![b'z'; MAX_RAW_BYTES * 2]);
        assert!(
            s.raw.capacity() <= MAX_RAW_BYTES + MAX_RAW_BYTES / 2,
            "retained {} bytes of capacity for a {MAX_RAW_BYTES} byte ceiling",
            s.raw.capacity()
        );
    }

    #[test]
    fn ansi_colour_is_interpreted_not_printed() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        s.process(b"\x1b[31mRED\x1b[0m\r\n");
        let visible = s.visible_lines();
        assert!(visible.iter().any(|l| l.contains("RED")));
        assert!(
            !visible.iter().any(|l| l.contains("\x1b")),
            "escape sequences should be consumed by the emulator"
        );
        let cell = s.screen().cell(0, 0).unwrap();
        assert_eq!(cell.fgcolor(), vt100::Color::Idx(1));
    }

    #[test]
    fn carriage_returns_rewrite_the_line() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        s.process(b"50%\r100%\r\n");
        let visible = s.visible_lines();
        assert!(visible.iter().any(|l| l.starts_with("100%")));
    }

    #[test]
    fn resizing_is_idempotent_and_clamps_to_a_safe_floor() {
        let mut s = store_with(5);
        s.resize(20, 60);
        assert_eq!(s.screen().size(), (20, 60));
        // vt100 underflows on very narrow grids, so the floor is a crash guard.
        s.resize(0, 0);
        assert_eq!(s.screen().size(), (MIN_ROWS, MIN_COLS));
    }

    #[test]
    fn collapsing_to_nothing_does_not_panic_while_replaying() {
        // Regression: rebuilding the parser replays retained output, and
        // replaying into a one-column grid panicked inside vt100.
        let mut s = store_with(40);
        for cols in [1, 2, 5, 19, 20, 21, 200, 1] {
            s.resize(1, cols);
        }
        assert!(s.screen().size().1 >= MIN_COLS);
    }

    #[test]
    fn all_text_does_not_duplicate_lines_at_an_uneven_scrollback_depth() {
        // 45 lines in a 10-row window leaves 35 rows of scrollback, which is not
        // a multiple of the window height - the case where a naive chunked walk
        // re-reads overlapping windows.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        for i in 0..45 {
            s.process(format!("line {i}\r\n").as_bytes());
        }
        let text = s.all_text();
        for i in 0..45 {
            let needle = format!("line {i}");
            let hits = text.lines().filter(|l| l.trim_end() == needle).count();
            assert_eq!(hits, 1, "expected exactly one 'line {i}', found {hits}");
        }
    }

    #[test]
    fn all_text_covers_more_than_the_retained_window() {
        // Beyond DEFAULT_SCROLLBACK the oldest lines are evicted, but everything
        // still retained must appear exactly once.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        for i in 0..1500 {
            s.process(format!("line {i}\r\n").as_bytes());
        }
        let text = s.all_text();
        assert!(
            !text.contains("line 0\n"),
            "oldest lines should have been evicted"
        );
        assert!(text.contains("line 1499"), "newest line must be present");
        assert_eq!(
            text.lines().filter(|l| l.trim_end() == "line 1400").count(),
            1
        );
        assert_eq!(s.scroll_offset(), 0, "copying must not move the view");
    }

    #[test]
    fn all_text_includes_scrolled_off_lines() {
        let mut s = store_with(40);
        let text = s.all_text();
        assert!(text.contains("line 0"), "expected scrollback in the copy");
        assert!(text.contains("line 39"), "expected the newest line too");
        assert!(tailing(&s), "copying must not disturb the scroll position");
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    /// Visible rows with blank padding removed.
    fn text(store: &mut LogStore) -> String {
        store.all_text()
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

        /// The invariant behind three separate defects: widening must never be
        /// an *additional* source of loss on top of ordinary eviction.
        ///
        /// Only widening. Retention is counted in rows, so narrowing legitimately
        /// evicts content -- the same text needs more rows once it wraps harder,
        /// and the oldest of it falls out of a fixed-row scrollback exactly as it
        /// would in a real terminal. The property test found that distinction
        /// before I did.
        ///
        /// Example-based tests could not cover this: the byte cost of a visual
        /// row depends on content an author has to think to write, which is how
        /// colour-dense output slipped past five hand-written cases.
        #[test]
        fn widening_never_drops_content_that_was_still_present(
            n_lines in 5usize..300,
            filler in 1usize..120,
            colourise in prop::bool::ANY,
            scrollback in 20usize..300,
            cols_before in 20u16..120,
            widen_by in 0u16..80,
        ) {
            let cols_after = cols_before.saturating_add(widen_by);
            let mut store = LogStore::new(scrollback);
            store.resize(24, cols_before);
            for i in 0..n_lines {
                let mut line = format!("MARK{i:05} ");
                for c in 0..filler {
                    if colourise {
                        line.push_str(if c % 2 == 0 { "\x1b[31m" } else { "\x1b[32m" });
                    }
                    line.push('x');
                }
                store.process(format!("{line}\n").as_bytes());
            }

            let before = text(&mut store);
            store.resize(24, cols_after);
            let after = text(&mut store);

            for i in 0..n_lines {
                let marker = format!("MARK{i:05}");
                if before.contains(&marker) {
                    prop_assert!(after.contains(&marker), "widening dropped {marker}");
                }
            }

            // Whichever way the pane moves, the newest line is never the one lost.
            let newest = format!("MARK{:05}", n_lines - 1);
            prop_assert!(after.contains(&newest), "newest line lost: {newest}");
        }

        /// The carried pen must reproduce every attribute the emulator tracks,
        /// compared as a whole rather than one the author remembered to check.
        /// This is what makes a missing attribute fail without anyone writing a
        /// test for that specific attribute.
        #[test]
        fn the_carried_pen_reproduces_every_attribute(
            codes in prop::collection::vec(0usize..11, 1..24),
            split_pct in 0usize..=100,
        ) {
            const SGR: [&str; 11] = [
                "\x1b[0m", "\x1b[1m", "\x1b[2m", "\x1b[3m", "\x1b[4m", "\x1b[7m",
                "\x1b[31m", "\x1b[42m", "\x1b[38;5;200m", "\x1b[48;2;10;20;30m", "\x1b[91m",
            ];
            fn attrs(cell: &vt100::Cell) -> (bool, bool, bool, bool, bool, vt100::Color, vt100::Color) {
                (
                    cell.bold(), cell.dim(), cell.italic(), cell.underline(),
                    cell.inverse(), cell.fgcolor(), cell.bgcolor(),
                )
            }

            let bytes: Vec<u8> = codes.iter().map(|i| SGR[*i]).collect::<String>().into_bytes();
            let split = bytes.len() * split_pct / 100;
            let (prefix, dropped) = bytes.split_at(split);

            let mut reference = vt100::Parser::new(MIN_ROWS, MIN_COLS, 0);
            reference.process(prefix);
            reference.process(dropped);
            reference.process(b"x");
            let want = attrs(reference.screen().cell(0, 0).unwrap());

            let mut replayed = vt100::Parser::new(MIN_ROWS, MIN_COLS, 0);
            replayed.process(&pen_after(prefix, dropped));
            replayed.process(b"x");
            let got = attrs(replayed.screen().cell(0, 0).unwrap());

            prop_assert_eq!(got, want);
        }

        /// Footprint stays bounded for arbitrary writes, rather than for the one
        /// multiplier a bug report happened to use.
        #[test]
        fn the_retained_buffer_stays_bounded(
            chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..2000), 1..24),
            scrollback in 4usize..48,
            rows in 3u16..24,
            cols in 20u16..120,
        ) {
            let mut store = LogStore::new(scrollback);
            store.resize(rows, cols);
            for chunk in &chunks {
                store.process(chunk);
                prop_assert!(store.raw.len() <= MAX_RAW_BYTES);
                prop_assert!(
                    store.raw.capacity() <= MAX_RAW_BYTES.saturating_mul(2).max(4096),
                    "capacity {} grew unboundedly",
                    store.raw.capacity()
                );
                prop_assert!(
                    !store.raw.is_empty() || !store.has_output(),
                    "replay data was discarded wholesale"
                );
                prop_assert!(
                    store.lines <= store.keep_lines.saturating_mul(2),
                    "line budget exceeded: {} retained against a budget of {}",
                    store.lines,
                    store.keep_lines
                );
            }
        }

        /// Arbitrary bytes and arbitrary geometry, in any order, must not panic.
        /// Generalises a fixed list of widths that once guarded a vt100
        /// underflow.
        #[test]
        fn arbitrary_input_and_geometry_never_panics(
            chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..600), 0..12),
            sizes in prop::collection::vec((0u16..200, 0u16..200), 1..8),
            scrollback in 0usize..64,
        ) {
            let mut store = LogStore::new(scrollback);
            for (rows, cols) in &sizes {
                store.resize(*rows, *cols);
                for chunk in &chunks {
                    store.process(chunk);
                }
                let _ = store.all_text();
            }
        }
    }
}
