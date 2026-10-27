//! The inline "live region" renderer.
//!
//! Claude Code does not take over the terminal with an alternate screen. Its
//! finished output flows into the terminal's own scrollback, and only a small
//! region at the bottom (spinner, prompt box, footer) is redrawn in place.
//! [`LiveScreen`] is that mechanism, and it is the *only* thing in the process
//! that writes to the terminal while a session is open.
//!
//! The rules that keep it from breaking:
//!
//! * One writer. Everything else sends messages; nothing else prints.
//! * Row arithmetic is relative. We never ask the terminal where the cursor is;
//!   we remember how many rows below the region's top we left it, so scrolling,
//!   a full screen and a shell prompt above us all just work.
//! * Every live line is at most `cols` cells (the caller composes to width and
//!   we truncate as a backstop), so a line is exactly one terminal row.
//! * Raw mode is assumed, so every line break is an explicit `\r\n`.
//! * A frame is written inside a synchronized-update block with the cursor
//!   hidden, so nothing flickers and nothing half-drawn is ever visible.

use std::io::{self, Write};

use super::text::{truncate, width};

/// One snapshot of the live region.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    /// Rows of the region, top to bottom. Each must be a single line.
    pub lines: Vec<String>,
    /// Where the text cursor goes: `(row, col)` inside `lines`. `None` hides it.
    pub cursor: Option<(usize, usize)>,
}

const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";

pub struct LiveScreen<W: Write> {
    out: W,
    cols: usize,
    rows: usize,
    /// The lines currently on screen, as drawn.
    drawn: Vec<String>,
    /// Row (0 = region top) where the terminal's cursor was left.
    cursor_row: usize,
    /// Column where the terminal's cursor was left.
    cursor_col: usize,
    /// What the last frame asked for, to skip identical redraws.
    last: Option<Frame>,
}

impl<W: Write> LiveScreen<W> {
    pub fn new(out: W, cols: usize, rows: usize) -> Self {
        Self {
            out,
            cols: cols.max(1),
            rows: rows.max(2),
            drawn: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
            last: None,
        }
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    #[cfg(test)]
    pub fn writer(&self) -> &W {
        &self.out
    }

    #[cfg(test)]
    pub fn writer_mut(&mut self) -> &mut W {
        &mut self.out
    }

    /// The tallest region we can keep on screen. One row is left so that the
    /// region never needs to scroll its own top out of reach.
    pub fn max_height(&self) -> usize {
        self.rows.saturating_sub(1).max(1)
    }

    /// Redraw the live region.
    pub fn draw(&mut self, frame: &Frame) -> io::Result<()> {
        if self.last.as_ref() == Some(frame) {
            return Ok(());
        }
        let mut buf = String::new();
        buf.push_str(SYNC_BEGIN);
        buf.push_str(HIDE_CURSOR);
        self.erase(&mut buf);
        self.paint(frame, &mut buf);
        buf.push_str(SYNC_END);
        self.flush(&buf)
    }

    /// Commit `lines` above the live region (they become scrollback), then
    /// redraw the region beneath them.
    pub fn print(&mut self, lines: &[String], frame: &Frame) -> io::Result<()> {
        if lines.is_empty() {
            return self.draw(frame);
        }
        let mut buf = String::new();
        buf.push_str(SYNC_BEGIN);
        buf.push_str(HIDE_CURSOR);
        self.erase(&mut buf);
        for line in lines {
            // Static rows may be wider than the terminal; the terminal wraps
            // them and nothing depends on their height.
            buf.push_str(&line.replace('\n', " "));
            buf.push_str("\r\n");
        }
        self.paint(frame, &mut buf);
        buf.push_str(SYNC_END);
        self.flush(&buf)
    }

    /// The terminal changed size. Terminals re-wrap what is already on screen,
    /// so work out how many rows our region now spans before wiping it.
    pub fn resize(&mut self, cols: usize, rows: usize, frame: &Frame) -> io::Result<()> {
        let (cols, rows) = (cols.max(1), rows.max(2));
        let mut up = 0usize;
        if !self.drawn.is_empty() {
            for line in self.drawn.iter().take(self.cursor_row) {
                up += span(width(line), cols);
            }
            up += self.cursor_col / cols;
        }
        // Never reach above the screen even if the estimate says so.
        let up = up.min(rows.saturating_sub(1));
        let mut buf = String::new();
        buf.push_str(SYNC_BEGIN);
        buf.push_str(HIDE_CURSOR);
        buf.push('\r');
        if up > 0 {
            buf.push_str(&format!("\x1b[{up}A"));
        }
        buf.push_str("\x1b[J");
        self.cols = cols;
        self.rows = rows;
        self.drawn.clear();
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.last = None;
        self.paint(frame, &mut buf);
        buf.push_str(SYNC_END);
        self.flush(&buf)
    }

    /// Wipe the visible screen (Ctrl+L). Scrollback is left alone.
    pub fn clear_screen(&mut self, frame: &Frame) -> io::Result<()> {
        let mut buf = String::new();
        buf.push_str(SYNC_BEGIN);
        buf.push_str(HIDE_CURSOR);
        buf.push_str("\x1b[2J\x1b[H");
        self.drawn.clear();
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.last = None;
        self.paint(frame, &mut buf);
        buf.push_str(SYNC_END);
        self.flush(&buf)
    }

    /// Remove the region and leave the cursor at the start of a clean row, so
    /// the shell prompt that follows lands where the region began.
    pub fn finish(&mut self) -> io::Result<()> {
        let mut buf = String::new();
        buf.push_str(HIDE_CURSOR);
        self.erase(&mut buf);
        buf.push_str(SHOW_CURSOR);
        self.drawn.clear();
        self.last = None;
        self.flush(&buf)
    }

    /// Move to the region's first row and blank what we drew there.
    ///
    /// The rows are erased one at a time (`ESC[2K`) rather than with "clear to
    /// end of screen" (`ESC[J`). tmux treats `ESC[J` at the home position as a
    /// whole-screen clear and files the old rows into scrollback, which left a
    /// stale prompt box above the welcome banner. `ESC[J` is only used when we
    /// do not know what is on screen (the first paint, after a resize).
    fn erase(&mut self, buf: &mut String) {
        buf.push('\r');
        if self.cursor_row > 0 {
            buf.push_str(&format!("\x1b[{}A", self.cursor_row));
        }
        let rows = self.drawn.len();
        if rows == 0 {
            buf.push_str("\x1b[J");
        } else {
            for index in 0..rows {
                if index > 0 {
                    // CUD never scrolls, unlike a line feed on the last row.
                    buf.push_str("\x1b[B");
                }
                buf.push_str("\x1b[2K");
            }
            if rows > 1 {
                buf.push_str(&format!("\x1b[{}A", rows - 1));
            }
            buf.push('\r');
        }
        self.drawn.clear();
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    /// Write the frame at the cursor (which must be at the region's top) and
    /// park the text cursor.
    fn paint(&mut self, frame: &Frame, buf: &mut String) {
        let max = self.max_height();
        let skip = frame.lines.len().saturating_sub(max);
        let lines: Vec<String> = frame.lines[skip..]
            .iter()
            .map(|line| {
                let flat = line.replace('\n', " ");
                if width(&flat) > self.cols { truncate(&flat, self.cols) } else { flat }
            })
            .collect();
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                buf.push_str("\r\n");
            }
            buf.push_str(line);
        }
        let last_row = lines.len().saturating_sub(1);
        self.cursor_row = last_row;
        self.cursor_col = lines.last().map(|line| width(line)).unwrap_or(0).min(self.cols);
        match frame.cursor.and_then(|(row, col)| row.checked_sub(skip).map(|row| (row, col))) {
            Some((row, col)) if row <= last_row => {
                let col = col.min(self.cols.saturating_sub(1));
                buf.push('\r');
                if last_row > row {
                    buf.push_str(&format!("\x1b[{}A", last_row - row));
                }
                if col > 0 {
                    buf.push_str(&format!("\x1b[{col}C"));
                }
                buf.push_str(SHOW_CURSOR);
                self.cursor_row = row;
                self.cursor_col = col;
            }
            _ => buf.push_str(HIDE_CURSOR),
        }
        self.drawn = lines;
        self.last = Some(frame.clone());
    }

    fn flush(&mut self, buf: &str) -> io::Result<()> {
        self.out.write_all(buf.as_bytes())?;
        self.out.flush()
    }
}

/// Terminal rows a line of `cells` cells occupies at `cols` columns.
fn span(cells: usize, cols: usize) -> usize {
    cells.div_ceil(cols).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz(value: u16) -> std::num::NonZeroU16 {
        std::num::NonZeroU16::new(value).unwrap()
    }

    /// A real terminal emulator behind the screen: what a user would see.
    struct Term {
        screen: LiveScreen<Vec<u8>>,
        parser: vt100::Parser,
    }

    impl Term {
        fn new(cols: u16, rows: u16) -> Self {
            Self {
                screen: LiveScreen::new(Vec::new(), cols as usize, rows as usize),
                parser: vt100::Parser::new(nz(rows), nz(cols), 200),
            }
        }

        fn sync(&mut self) {
            let bytes = std::mem::take(self.screen.writer_mut());
            self.parser.process(&bytes);
        }

        fn frame(lines: &[&str], cursor: Option<(usize, usize)>) -> Frame {
            Frame { lines: lines.iter().map(|line| line.to_string()).collect(), cursor }
        }

        fn draw(&mut self, frame: &Frame) {
            self.screen.draw(frame).unwrap();
            self.sync();
        }

        fn print(&mut self, lines: &[&str], frame: &Frame) {
            let lines: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
            self.screen.print(&lines, frame).unwrap();
            self.sync();
        }

        /// Visible rows, trailing blanks trimmed.
        fn rows(&self) -> Vec<String> {
            let screen = self.parser.screen();
            let mut rows: Vec<String> = screen.rows(0, screen.size().1.get()).collect();
            while rows.last().is_some_and(|row| row.trim().is_empty()) {
                rows.pop();
            }
            rows.into_iter().map(|row| row.trim_end().to_string()).collect()
        }

        fn cursor(&self) -> (u16, u16) {
            self.parser.screen().cursor_position()
        }

        fn cursor_hidden(&self) -> bool {
            self.parser.screen().hide_cursor()
        }
    }

    #[test]
    fn a_frame_paints_at_the_top_and_parks_the_cursor() {
        let mut term = Term::new(20, 10);
        term.draw(&Term::frame(&["spinner", "> hello", "footer"], Some((1, 7))));
        assert_eq!(term.rows(), vec!["spinner", "> hello", "footer"]);
        assert_eq!(term.cursor(), (1, 7));
        assert!(!term.cursor_hidden());
    }

    #[test]
    fn redrawing_replaces_the_region_without_leaving_rows_behind() {
        let mut term = Term::new(20, 10);
        term.draw(&Term::frame(&["a", "b", "c", "d"], Some((3, 1))));
        term.draw(&Term::frame(&["x", "y"], Some((1, 1))));
        assert_eq!(term.rows(), vec!["x", "y"], "a shrinking region must clear its old rows");
        term.draw(&Term::frame(&["1", "2", "3", "4", "5"], Some((4, 1))));
        assert_eq!(term.rows(), vec!["1", "2", "3", "4", "5"]);
    }

    #[test]
    fn printing_commits_lines_above_and_the_region_follows() {
        let mut term = Term::new(20, 10);
        let frame = Term::frame(&["> ", "footer"], Some((0, 2)));
        term.draw(&frame);
        term.print(&["first line", "second line"], &frame);
        assert_eq!(term.rows(), vec!["first line", "second line", ">", "footer"]);
        term.print(&["third"], &frame);
        assert_eq!(
            term.rows(),
            vec!["first line", "second line", "third", ">", "footer"],
            "the prompt stays below the transcript, never beside it"
        );
        assert_eq!(term.cursor(), (3, 2));
    }

    #[test]
    fn a_full_screen_scrolls_old_lines_into_scrollback_and_nothing_tears() {
        let mut term = Term::new(20, 6);
        let frame = Term::frame(&["> typing", "footer"], Some((0, 8)));
        term.draw(&frame);
        for n in 0..12 {
            let line = format!("line {n}");
            term.print(&[line.as_str()], &frame);
        }
        let rows = term.rows();
        assert_eq!(rows.len(), 6, "{rows:?}");
        assert_eq!(&rows[4..], ["> typing", "footer"]);
        assert_eq!(&rows[..4], ["line 8", "line 9", "line 10", "line 11"]);
        // The scrolled-off lines are in the terminal's own history, in order.
        let mut history = Vec::new();
        for back in (1..=term.parser.screen().scrollback_len()).rev() {
            term.parser.screen_mut().set_scrollback(back);
            history.push(term.parser.screen().rows(0, 6).next().unwrap().trim_end().to_string());
        }
        term.parser.screen_mut().set_scrollback(0);
        assert_eq!(history.first().map(String::as_str), Some("line 0"), "{history:?}");
        assert!(history.contains(&"line 7".to_string()), "{history:?}");
    }

    #[test]
    fn a_growing_region_at_the_bottom_of_the_screen_scrolls_instead_of_tearing() {
        let mut term = Term::new(20, 8);
        let small = Term::frame(&["> ", "footer"], Some((0, 2)));
        for n in 0..8 {
            let line = format!("old {n}");
            term.print(&[line.as_str()], &small);
        }
        // The screen is full; now the prompt grows by three rows.
        let tall = Term::frame(&["spin", "---", "> a", "  b", "  c", "footer"], Some((4, 3)));
        term.draw(&tall);
        let rows = term.rows();
        assert_eq!(&rows[rows.len() - 6..], ["spin", "---", "> a", "  b", "  c", "footer"]);
        // And back down: still no residue.
        term.draw(&small);
        let rows = term.rows();
        assert_eq!(&rows[rows.len() - 2..], [">", "footer"], "{rows:?}");
        assert!(!rows.iter().any(|row| row == "spin" || row == "---"), "{rows:?}");
    }

    #[test]
    fn identical_frames_write_nothing() {
        let mut term = Term::new(20, 10);
        let frame = Term::frame(&["> hi"], Some((0, 4)));
        term.draw(&frame);
        term.screen.draw(&frame).unwrap();
        assert!(term.screen.writer().is_empty(), "an unchanged frame must not repaint");
    }

    #[test]
    fn overwide_lines_are_cut_to_the_terminal_width() {
        let mut term = Term::new(10, 6);
        term.draw(&Term::frame(&["0123456789ABCDEF", "next"], Some((1, 4))));
        assert_eq!(term.rows(), vec!["012345678\u{2026}", "next"]);
        assert_eq!(term.cursor(), (1, 4));
    }

    #[test]
    fn a_region_taller_than_the_screen_keeps_its_bottom() {
        let mut term = Term::new(20, 4);
        let lines: Vec<String> = (0..9).map(|n| format!("row {n}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        term.draw(&Term::frame(&refs, Some((8, 2))));
        let rows = term.rows();
        assert_eq!(rows, vec!["row 6", "row 7", "row 8"], "the prompt end must stay visible");
        assert_eq!(term.cursor(), (2, 2));
    }

    #[test]
    fn a_hidden_cursor_stays_hidden() {
        let mut term = Term::new(20, 6);
        term.draw(&Term::frame(&["dialog"], None));
        assert!(term.cursor_hidden());
        term.draw(&Term::frame(&["> "], Some((0, 2))));
        assert!(!term.cursor_hidden());
    }

    #[test]
    fn wide_characters_park_the_cursor_by_cells() {
        let mut term = Term::new(20, 6);
        term.draw(&Term::frame(&["> \u{4f60}\u{597d}x"], Some((0, 7))));
        assert_eq!(term.cursor(), (0, 7));
    }

    #[test]
    fn widening_the_terminal_redraws_cleanly() {
        let mut term = Term::new(20, 8);
        let frame = Term::frame(&["spin", "> typed", "footer"], Some((1, 7)));
        term.print(&["history"], &frame);
        term.screen.resize(30, 8, &frame).unwrap();
        term.parser.screen_mut().set_size(nz(8), nz(30));
        term.sync();
        assert_eq!(term.rows(), vec!["history", "spin", "> typed", "footer"]);
        assert_eq!(term.cursor(), (2, 7));
    }

    #[test]
    fn finish_clears_the_region_and_leaves_a_clean_row() {
        let mut term = Term::new(20, 8);
        let frame = Term::frame(&["> half typed", "footer"], Some((0, 12)));
        term.print(&["transcript"], &frame);
        term.screen.finish().unwrap();
        term.sync();
        assert_eq!(term.rows(), vec!["transcript"]);
        assert_eq!(term.cursor(), (1, 0), "the shell prompt goes on the row after the transcript");
        assert!(!term.cursor_hidden());
    }

    #[test]
    fn every_line_break_is_an_explicit_carriage_return() {
        let mut term = Term::new(20, 8);
        term.draw(&Term::frame(&["a", "b"], None));
        let mut screen = LiveScreen::new(Vec::new(), 20, 8);
        screen.print(&["x".to_string(), "y".to_string()], &Frame::default()).unwrap();
        let text = String::from_utf8(screen.writer().clone()).unwrap();
        assert!(!text.replace("\r\n", "").contains('\n'), "bare LF would stair-step in raw mode: {text:?}");
    }

    #[test]
    fn clear_screen_homes_and_redraws() {
        let mut term = Term::new(20, 6);
        let frame = Term::frame(&["> ", "footer"], Some((0, 2)));
        term.print(&["one", "two", "three"], &frame);
        term.screen.clear_screen(&frame).unwrap();
        term.sync();
        assert_eq!(term.rows(), vec![">", "footer"]);
        assert_eq!(term.cursor(), (0, 2));
    }

    /// Regression: erasing a known region with `ESC[J` made tmux file the old
    /// frame into scrollback when the region sat at the top-left of the pane.
    #[test]
    fn a_known_region_is_erased_row_by_row_never_with_clear_to_end_of_screen() {
        let mut screen = LiveScreen::new(Vec::new(), 40, 12);
        let frame = Frame { lines: vec!["a".into(), "b".into(), "c".into()], cursor: Some((1, 1)) };
        screen.draw(&frame).unwrap();
        screen.writer_mut().clear();
        screen.draw(&Frame { lines: vec!["x".into(), "y".into()], cursor: None }).unwrap();
        let text = String::from_utf8(screen.writer().clone()).unwrap();
        assert!(!text.contains("\x1b[J"), "an unknown-extent clear on a known region: {text:?}");
        assert_eq!(text.matches("\x1b[2K").count(), 3, "one erase per row that was drawn: {text:?}");
        screen.writer_mut().clear();
        screen.print(&["committed".to_string()], &Frame::default()).unwrap();
        assert!(!String::from_utf8(screen.writer().clone()).unwrap().contains("\x1b[J"));
    }

    #[test]
    fn a_synchronized_update_wraps_every_frame() {
        let mut screen = LiveScreen::new(Vec::new(), 20, 8);
        screen.draw(&Frame { lines: vec!["x".into()], cursor: None }).unwrap();
        let text = String::from_utf8(screen.writer().clone()).unwrap();
        assert!(text.starts_with(SYNC_BEGIN) && text.ends_with(SYNC_END), "{text:?}");
    }
}
