use anyhow::Result;
use libghostty_vt::render::{CellIterator, RowIterator};
use libghostty_vt::style::{StyleColor, Underline};
use libghostty_vt::terminal::ScrollViewport;
use libghostty_vt::terminal::{
    ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType,
    PrimaryDeviceAttributes, SecondaryDeviceAttributes, TertiaryDeviceAttributes,
};
use libghostty_vt::{RenderState, Terminal, TerminalOptions};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::rc::Rc;

/// One colored attribute: unset falls back to the pane's default.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellColor {
    Default,
    /// 0-255 palette index, as the terminal set it.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Per-run text attributes (SGR state at the moment the run was written).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellAttrs {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub inverse: bool,
}

/// A run of same-styled text within one line.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct StyledRun {
    pub text: String,
    pub fg: CellColor,
    pub bg: CellColor,
    pub attrs: CellAttrs,
}

/// One viewport row as styled runs, matching the pane's `text` line for
/// line.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct StyledLine {
    pub runs: Vec<StyledRun>,
}

/// Where to scroll the pane's viewport. Mirrors the protocol type so
/// the daemon can route a client's scroll request to a pane worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollTarget {
    Delta(isize),
    /// Absolute row offset from the top of scrollback (search jump).
    Row(usize),
    Top,
    Bottom,
}

/// The pane's viewport position inside its scrollback. `None` when the
/// viewport is pinned to the bottom (live-follow).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollPos {
    /// Rows the viewport top is above the bottom of the active screen.
    pub offset: usize,
    /// Total scrollback rows.
    pub total: usize,
}

/// One pane's terminal state. `Terminal` is !Send: Emulator must be
/// created and used on a single thread (the daemon core thread in v0.1).
pub struct Emulator {
    // pub(crate) so sibling modules (search) can walk the grid without
    // Emulator re-exporting every Terminal method.
    pub(crate) terminal: Terminal<'static, 'static>,
    /// Reply bytes the emulator wants written back to the pane's PTY
    /// (terminal query responses such as DA1). The on_pty_write callback
    /// appends here; the daemon drains via take_pty_writes.
    pty_writes: Rc<RefCell<Vec<Vec<u8>>>>,
}

impl Emulator {
    pub fn new(cols: u16, rows: u16) -> Result<Self> {
        let opts = TerminalOptions {
            cols,
            rows,
            max_scrollback: 10_000,
        };
        let mut terminal = Terminal::new(opts)?;
        // Programs query the terminal on startup (fish waits up to ten
        // seconds for a DA1 answer). Claim a VT220 with color so shells
        // proceed immediately; libghostty composes the reply bytes.
        terminal.on_device_attributes(move |_term| {
            Some(DeviceAttributes {
                primary: PrimaryDeviceAttributes::new(
                    ConformanceLevel::VT220,
                    &[DeviceAttributeFeature::ANSI_COLOR],
                ),
                secondary: SecondaryDeviceAttributes {
                    device_type: DeviceType::VT220,
                    firmware_version: 1,
                    rom_cartridge: 0,
                },
                tertiary: TertiaryDeviceAttributes { unit_id: 0 },
            })
        })?;
        // Every terminal-query response the core generates (DA1, DECRQM,
        // DSR) flows through this callback; collect it for the daemon.
        let pty_writes: Rc<RefCell<Vec<Vec<u8>>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&pty_writes);
        terminal.on_pty_write(move |_term, data| {
            sink.borrow_mut().push(data.to_vec());
        })?;
        Ok(Self {
            terminal,
            pty_writes,
        })
    }

    /// Bytes straight from the PTY.
    pub fn feed(&mut self, data: &[u8]) {
        self.terminal.vt_write(data);
    }

    /// Take the accumulated query replies; the daemon writes them back
    /// to the pane's PTY.
    pub fn take_pty_writes(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.pty_writes.borrow_mut())
    }

    /// Resize the viewport; libghostty reflows the primary screen.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.terminal.resize(cols, rows, 0, 0)?;
        Ok(())
    }

    /// Scroll the pane's viewport. libghostty keeps the viewport sticky
    /// while scrolled up: new output does not force it back down until
    /// a `Bottom` scroll (verified in `viewport_sticks_while_scrolled_up`).
    pub fn scroll(&mut self, target: ScrollTarget) {
        let sv = match target {
            ScrollTarget::Delta(d) => ScrollViewport::Delta(d),
            ScrollTarget::Row(r) => ScrollViewport::Row(r),
            ScrollTarget::Top => ScrollViewport::Top,
            ScrollTarget::Bottom => ScrollViewport::Bottom,
        };
        self.terminal.scroll_viewport(sv);
    }

    /// The viewport position inside the scrollback, `None` when pinned
    /// to the bottom (live-follow). libghostty reports the position via
    /// the scrollbar track geometry.
    pub fn scroll_position(&mut self) -> Result<Option<ScrollPos>> {
        let sb = self.terminal.scrollbar()?;
        // At the bottom the scrollbar offset sits at total - len.
        if sb.offset + sb.len >= sb.total {
            return Ok(None);
        }
        let total_rows = self.terminal.scrollback_rows()?;
        Ok(Some(ScrollPos {
            offset: sb.offset as usize,
            total: total_rows,
        }))
    }

    /// The viewport top row in screen space, even when pinned to the
    /// bottom. `scroll_position` collapses that case to `None`, but
    /// prompt jumps need the real resume row: a `Row` scroll to a
    /// prompt inside the visible screen clamps to the bottom, and the
    /// next jump must not restart from "no history".
    pub fn viewport_offset(&mut self) -> Result<usize> {
        Ok(self.terminal.scrollbar()?.offset as usize)
    }

    /// Whether the pane asked for application cursor keys (DECCKM, mode
    /// 1). Arrows must arrive as ESC O A..D instead of ESC [ A..D then,
    /// or vim and friends ignore them.
    pub fn app_cursor(&mut self) -> Result<bool> {
        Ok(self.terminal.mode(libghostty_vt::terminal::Mode::DECCKM)?)
    }

    /// Erase every scrollback line (CSI 3 J) and, since real terminal
    /// semantics never touch the visible grid, also erase the visible
    /// screen above the cursor's row so content that scrolled onto
    /// screen just before the clear does not linger (plan Task 10). The
    /// cursor's row and everything below it, including an in-progress
    /// typed command, stays untouched.
    ///
    /// The cursor's row comes from `cursor_x`/`cursor_y`, which are
    /// active-screen relative - the same frame CSI H addresses - so this
    /// is independent of where the viewport happens to be scrolled.
    pub fn clear_history(&mut self) {
        self.terminal.vt_write(b"\x1b[3J");
        if !self.terminal.is_cursor_visible().unwrap_or(false) {
            return;
        }
        let (Ok(col), Ok(row)) = (self.terminal.cursor_x(), self.terminal.cursor_y()) else {
            return;
        };
        for r in 0..row {
            self.terminal
                .vt_write(format!("\x1b[{};1H\x1b[2K", r + 1).as_bytes());
        }
        self.terminal
            .vt_write(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
    }

    /// Cursor cell position within the viewport, when visible.
    pub fn cursor(&mut self) -> Result<Option<(u16, u16)>> {
        let mut render_state = RenderState::new()?;
        let snapshot = render_state.update(&self.terminal)?;
        if !snapshot.cursor_visible()? {
            return Ok(None);
        }
        Ok(snapshot.cursor_viewport()?.map(|c| (c.x, c.y)))
    }

    /// Current viewport as plain text, one line per row.
    pub fn screen_text(&mut self) -> Result<String> {
        let mut render_state = RenderState::new()?;
        let snapshot = render_state.update(&self.terminal)?;
        let mut rows = RowIterator::new()?;
        let mut cells = CellIterator::new()?;
        let mut out = String::new();
        let mut row_iter = rows.update(&snapshot)?;
        while let Some(row) = row_iter.next() {
            let mut line = String::new();
            let mut cell_iter = cells.update(row)?;
            while let Some(cell) = cell_iter.next() {
                for g in cell.graphemes()? {
                    line.push(g);
                }
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        Ok(out)
    }

    /// Current viewport as styled runs, one `StyledLine` per row. Same
    /// walk as `screen_text`, keeping each cell's colors and attributes;
    /// adjacent cells with equal styling merge into one run. Palette
    /// indices are preserved so the client resolves them against its own
    /// palette rather than the emulator's defaults.
    pub fn screen_lines(&mut self) -> Result<Vec<StyledLine>> {
        let mut render_state = RenderState::new()?;
        let snapshot = render_state.update(&self.terminal)?;
        let mut rows = RowIterator::new()?;
        let mut cells = CellIterator::new()?;
        let mut out = Vec::new();
        let mut row_iter = rows.update(&snapshot)?;
        while let Some(row) = row_iter.next() {
            let mut line = StyledLine { runs: Vec::new() };
            let mut cell_iter = cells.update(row)?;
            while let Some(cell) = cell_iter.next() {
                let style = cell.style()?;
                let fg = cell_color(style.fg_color);
                let bg = cell_color(style.bg_color);
                let attrs = CellAttrs {
                    bold: style.bold,
                    italic: style.italic,
                    underline: !matches!(style.underline, Underline::None),
                    strikethrough: style.strikethrough,
                    inverse: style.inverse,
                };
                let text: String = cell.graphemes()?.iter().collect();
                match line.runs.last_mut() {
                    Some(run) if run.fg == fg && run.bg == bg && run.attrs == attrs => {
                        run.text.push_str(&text);
                    }
                    _ => line.runs.push(StyledRun {
                        text,
                        fg,
                        bg,
                        attrs,
                    }),
                }
            }
            // Trailing default-styled whitespace carries no information.
            while let Some(last) = line.runs.last() {
                if last.fg == CellColor::Default
                    && last.bg == CellColor::Default
                    && last.attrs == CellAttrs::default()
                    && last.text.chars().all(|c| c == ' ' || c == '\0')
                {
                    line.runs.pop();
                } else {
                    break;
                }
            }
            out.push(line);
        }
        Ok(out)
    }
}

fn cell_color(sc: StyleColor) -> CellColor {
    match sc {
        StyleColor::None => CellColor::Default,
        StyleColor::Palette(idx) => CellColor::Indexed(idx.0),
        StyleColor::Rgb(c) => CellColor::Rgb(c.r, c.g, c.b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_plain_text() {
        let mut emu = Emulator::new(80, 24).unwrap();
        emu.feed(b"hello\r\n");
        assert!(emu.screen_text().unwrap().contains("hello"));
    }

    #[test]
    fn consumes_sgr_without_leaking_escapes() {
        let mut emu = Emulator::new(80, 24).unwrap();
        emu.feed(b"\x1b[1;32mworld\x1b[0m!\r\n");
        let text = emu.screen_text().unwrap();
        assert!(text.contains("world"));
        assert!(!text.contains('\x1b'));
    }

    #[test]
    fn wraps_at_width() {
        let mut emu = Emulator::new(10, 4).unwrap();
        emu.feed(b"abcdefghijklmnop");
        let text = emu.screen_text().unwrap();
        assert_eq!(text.lines().next().unwrap(), "abcdefghij");
        assert!(text.lines().nth(1).unwrap().starts_with("klmnop"));
    }

    #[test]
    fn resize_reflows_wrapped_text() {
        let mut emu = Emulator::new(10, 4).unwrap();
        emu.feed(b"abcdefghijklmnop");
        emu.resize(20, 4).unwrap();
        let text = emu.screen_text().unwrap();
        assert!(text.lines().next().unwrap().contains("abcdefghijklmnop"));
    }

    #[test]
    fn empty_feed_changes_nothing() {
        let mut emu = Emulator::new(80, 24).unwrap();
        emu.feed(b"hi\r\n");
        let before = emu.screen_text().unwrap();
        emu.feed(b"");
        assert_eq!(emu.screen_text().unwrap(), before);
    }

    #[test]
    fn escape_sequence_split_across_feeds_does_not_leak() {
        // The SGR prefix arrives in one PTY read, the rest in the next;
        // the emulator must hold the partial sequence, not print it.
        let mut emu = Emulator::new(80, 24).unwrap();
        emu.feed(b"\x1b[");
        emu.feed(b"1;31mred\x1b[0m\r\n");
        let text = emu.screen_text().unwrap();
        assert!(text.contains("red"));
        assert!(!text.contains('\x1b'), "escape byte leaked into text");
        assert!(!text.contains('['), "bracket leaked into text");
    }

    #[test]
    fn wide_grapheme_occupies_its_own_cells() {
        // CJK text is two cells wide; the renderer reads graphemes per
        // cell, so the string must survive a round trip intact.
        let mut emu = Emulator::new(20, 4).unwrap();
        emu.feed("こんにちは".as_bytes());
        let text = emu.screen_text().unwrap();
        assert!(text.contains("こんにちは"));
    }

    #[test]
    fn control_bytes_other_than_newline_do_not_corrupt_text() {
        let mut emu = Emulator::new(80, 24).unwrap();
        emu.feed(b"start\x07mid\x08end\r\n");
        let text = emu.screen_text().unwrap();
        assert!(text.contains("start"), "got {text:?}");
        assert!(text.contains("end"), "got {text:?}");
    }

    #[test]
    fn shrink_then_grow_resize_keeps_text_readable() {
        let mut emu = Emulator::new(40, 10).unwrap();
        emu.feed(b"the quick brown fox jumps over the lazy dog\r\n");
        emu.resize(10, 10).unwrap();
        let small = emu.screen_text().unwrap();
        assert!(small.contains("the quick"), "shrunk view lost text");
        emu.resize(40, 10).unwrap();
        let back = emu.screen_text().unwrap();
        assert!(
            back.contains("jumps over the lazy") && back.lines().any(|l| l.contains("dog")),
            "grown view lost text: {back:?}"
        );
    }

    #[test]
    fn cursor_home_carriage_return_and_backspace_move_the_cursor() {
        let mut emu = Emulator::new(80, 24).unwrap();
        emu.feed(b"abcdef\x08\x08XY\r\n");
        let text = emu.screen_text().unwrap();
        // Two backspaces overwrite "ef" with "XY".
        assert!(text.contains("abcdXY"), "got {text:?}");
    }

    #[test]
    fn zero_size_emulator_is_rejected_or_harmless() {
        // A client could send Resize { cols: 0, rows: 0 }; the emulator
        // must either reject it or survive it, never panic.
        let mut emu = Emulator::new(80, 24).unwrap();
        // Survived: screen_text must not panic either. Rejected is fine too.
        if emu.resize(0, 0).is_ok() {
            let _ = emu.screen_text().unwrap();
        }
    }

    #[test]
    fn da1_query_produces_a_reply_for_the_pty() {
        // fish waits up to ten seconds for a DA1 answer; without a
        // registered on_device_attributes callback libghostty drops the
        // query silently. The reply must land in the pty_writes sink.
        let mut emu = Emulator::new(80, 24).unwrap();
        emu.feed(b"\x1b[c");
        let replies = emu.take_pty_writes();
        assert!(!replies.is_empty(), "no DA1 reply captured");
        let reply = replies.concat();
        assert!(reply.starts_with(b"\x1b["), "reply {reply:?} is not CSI");
        assert!(reply.ends_with(b"c"), "reply {reply:?} is not a DA1 form");
    }

    #[test]
    fn take_pty_writes_drains_the_sink() {
        let mut emu = Emulator::new(80, 24).unwrap();
        emu.feed(b"\x1b[c");
        assert!(!emu.take_pty_writes().is_empty());
        // A second drain with no new query returns nothing.
        assert!(emu.take_pty_writes().is_empty());
    }

    #[test]
    fn cursor_reports_the_visible_position() {
        let mut emu = Emulator::new(80, 24).unwrap();
        assert_eq!(
            emu.cursor().unwrap(),
            Some((0, 0)),
            "fresh screen shows home"
        );
        emu.feed(b"hello\r\nworld");
        let (x, y) = emu.cursor().unwrap().unwrap();
        assert_eq!((x, y), (5, 1), "cursor after two lines of text");
    }

    #[test]
    fn cursor_hides_when_a_program_conceals_it() {
        let mut emu = Emulator::new(80, 24).unwrap();
        emu.feed(b"\x1b[?25l");
        assert_eq!(emu.cursor().unwrap(), None);
    }

    #[test]
    fn scroll_up_shows_earlier_lines_and_reports_offset() {
        let mut emu = Emulator::new(80, 5).unwrap();
        for i in 1..=100 {
            emu.feed(format!("line{i}\r\n").as_bytes());
        }
        // Pinned to bottom: no scroll offset to report.
        assert_eq!(
            emu.scroll_position().unwrap(),
            None,
            "fresh feed is at bottom"
        );
        emu.scroll(ScrollTarget::Delta(-20));
        let pos = emu
            .scroll_position()
            .unwrap()
            .expect("scrolled up has a position");
        assert!(pos.offset > 0, "offset must be positive after scrolling up");
        assert_eq!(
            pos.total, 96,
            "total scrollback rows: 100 fed lines + prompt in 5-row screen"
        );
        let text = emu.screen_text().unwrap();
        assert!(
            text.lines().any(|l| l.starts_with("line")),
            "viewport shows history rows, got {text:?}"
        );
        assert!(
            !text.contains("line100"),
            "bottom row must not show while scrolled up"
        );
    }

    #[test]
    fn scroll_bottom_restores_live_follow() {
        let mut emu = Emulator::new(80, 5).unwrap();
        for i in 1..=100 {
            emu.feed(format!("line{i}\r\n").as_bytes());
        }
        emu.scroll(ScrollTarget::Delta(-20));
        assert!(emu.scroll_position().unwrap().is_some());
        emu.scroll(ScrollTarget::Bottom);
        assert_eq!(emu.scroll_position().unwrap(), None);
        let text = emu.screen_text().unwrap();
        assert!(
            text.contains("line100"),
            "bottom shows the latest line, got {text:?}"
        );
    }

    #[test]
    fn clear_history_also_clears_the_visible_screen_above_the_cursor() {
        let mut emu = Emulator::new(80, 5).unwrap();
        for i in 1..=100 {
            emu.feed(format!("line{i}\r\n").as_bytes());
        }
        // A command typed but not yet submitted: the cursor sits on the
        // visible screen's last row, and that row must survive the clear.
        emu.feed(b"prompt$ cmd");
        emu.clear_history();
        assert_eq!(
            emu.terminal.scrollback_rows().unwrap(),
            0,
            "CSI 3 J must still empty the scrollback"
        );
        let (cursor_col, cursor_row) = emu.cursor().unwrap().expect("cursor visible");
        let text = emu.screen_text().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[cursor_row as usize], "prompt$ cmd",
            "cursor's row must be untouched, got {lines:?}"
        );
        assert_eq!(cursor_col, 11, "cursor stays where typing left it");
        for row in lines.iter().take(cursor_row as usize) {
            assert!(
                row.is_empty(),
                "rows above the cursor must be cleared, got {lines:?}"
            );
        }
    }

    #[test]
    fn clear_history_empties_scrollback_and_the_screen_above_the_cursor() {
        // Contract narrowed by Task 10: the S3-6 acceptance criterion
        // ("active text is intact") only holds for the cursor's row and
        // below. Every line fed here ends in `\r\n`, so the cursor sits
        // on a blank row below "line100" - that row, and everything
        // above it, is now erased along with the scrollback.
        let mut emu = Emulator::new(80, 5).unwrap();
        for i in 1..=100 {
            emu.feed(format!("line{i}\r\n").as_bytes());
        }
        // Scrolled back so history exists and is visible.
        emu.scroll(ScrollTarget::Top);
        assert!(
            emu.terminal.scrollback_rows().unwrap() > 0,
            "scrollback must be populated before clearing"
        );
        emu.clear_history();
        assert_eq!(
            emu.terminal.scrollback_rows().unwrap(),
            0,
            "CSI 3 J must empty the scrollback"
        );
        emu.scroll(ScrollTarget::Bottom);
        let text = emu.screen_text().unwrap();
        assert!(
            !text.contains("line100"),
            "screen above the (blank) cursor row is erased too, got {text:?}"
        );
    }

    #[test]
    fn scroll_top_reaches_first_lines() {
        let mut emu = Emulator::new(80, 5).unwrap();
        for i in 1..=100 {
            emu.feed(format!("line{i}\r\n").as_bytes());
        }
        emu.scroll(ScrollTarget::Top);
        let text = emu.screen_text().unwrap();
        let first = text.lines().next().unwrap();
        assert!(
            first.starts_with("line1") || first.starts_with("line2"),
            "top shows the earliest rows, got {first:?}"
        );
    }

    #[test]
    fn viewport_sticks_while_scrolled_up() {
        // Record libghostty's stickiness: while the viewport is above the
        // bottom, new output must not force it back down.
        let mut emu = Emulator::new(80, 5).unwrap();
        for i in 1..=50 {
            emu.feed(format!("line{i}\r\n").as_bytes());
        }
        emu.scroll(ScrollTarget::Delta(-10));
        let before = emu.screen_text().unwrap();
        for i in 51..=60 {
            emu.feed(format!("line{i}\r\n").as_bytes());
        }
        let after = emu.screen_text().unwrap();
        assert_eq!(before, after, "new output moved a scrolled-up viewport");
        let pos = emu.scroll_position().unwrap().unwrap();
        assert_eq!(
            pos.offset, 36,
            "offset unchanged while stuck: got {}",
            pos.offset
        );
    }

    #[test]
    fn scrolling_above_top_clamps_or_rejects() {
        // A delta beyond the top must not panic or corrupt state.
        let mut emu = Emulator::new(80, 5).unwrap();
        for i in 1..=30 {
            emu.feed(format!("line{i}\r\n").as_bytes());
        }
        emu.scroll(ScrollTarget::Delta(-1000));
        let pos = emu.scroll_position().unwrap();
        assert!(pos.is_some(), "scrolled somewhere, position is reportable");
        let text = emu.screen_text().unwrap();
        assert!(
            text.lines().next().unwrap().contains("line1"),
            "clamped at top"
        );
    }

    #[test]
    fn screen_lines_match_screen_text_row_for_row() {
        let mut emu = Emulator::new(80, 5).unwrap();
        emu.feed(b"hello\r\nworld\r\n");
        let lines = emu.screen_lines().unwrap();
        let text = emu.screen_text().unwrap();
        for (line, row) in lines.iter().zip(text.lines()) {
            let joined: String = line.runs.iter().map(|r| r.text.as_str()).collect();
            assert_eq!(joined, row, "styled runs must reproduce the row");
        }
    }

    #[test]
    fn sgr_colors_and_attributes_land_in_runs() {
        let mut emu = Emulator::new(80, 5).unwrap();
        emu.feed(b"\x1b[1;31mred-bold\x1b[0m plain \x1b[4munder\x1b[0m\r\n");
        let lines = emu.screen_lines().unwrap();
        let runs = &lines[0].runs;
        let red = runs.iter().find(|r| r.text.contains("red-bold")).unwrap();
        assert_eq!(red.fg, CellColor::Indexed(1), "got {:?}", red.fg);
        assert!(red.attrs.bold);
        let plain = runs.iter().find(|r| r.text.contains("plain")).unwrap();
        assert_eq!(plain.fg, CellColor::Default);
        assert!(!plain.attrs.bold);
        let under = runs.iter().find(|r| r.text.contains("under")).unwrap();
        assert!(under.attrs.underline);
        assert!(!under.attrs.bold);
    }

    #[test]
    fn adjacent_same_style_cells_merge_into_one_run() {
        let mut emu = Emulator::new(80, 5).unwrap();
        emu.feed(b"\x1b[32mgreen-one\x1b[32m still green\x1b[0m\r\n");
        let lines = emu.screen_lines().unwrap();
        let green: Vec<_> = lines[0]
            .runs
            .iter()
            .filter(|r| r.fg == CellColor::Indexed(2))
            .collect();
        assert_eq!(
            green.len(),
            1,
            "same-style cells merge, got {:?}",
            lines[0].runs
        );
        assert_eq!(green[0].text.trim_end(), "green-one still green");
    }

    #[test]
    fn trailing_default_whitespace_is_trimmed_from_runs() {
        let mut emu = Emulator::new(80, 5).unwrap();
        emu.feed(b"text\r\n");
        let lines = emu.screen_lines().unwrap();
        let runs = &lines[0].runs;
        assert_eq!(runs.len(), 1, "no padded whitespace runs, got {runs:?}");
        assert_eq!(runs[0].text, "text");
    }

    #[test]
    fn background_color_lands_in_runs() {
        let mut emu = Emulator::new(80, 5).unwrap();
        emu.feed(b"\x1b[44mblue-bg\x1b[0m\r\n");
        let lines = emu.screen_lines().unwrap();
        let run = lines[0]
            .runs
            .iter()
            .find(|r| r.text.contains("blue"))
            .unwrap();
        assert_eq!(run.bg, CellColor::Indexed(4), "got {:?}", run.bg);
    }
}
