use anyhow::Result;
use libghostty_vt::render::{CellIterator, RowIterator};
use libghostty_vt::style::{Style, StyleColor, Underline};
use libghostty_vt::terminal::ScrollViewport;
use libghostty_vt::terminal::{
    ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Point, PointCoordinate,
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

    /// The pane's current grid width in columns.
    pub fn cols(&self) -> Result<u16> {
        Ok(self.terminal.cols()?)
    }

    /// The pane's current grid height in rows.
    pub fn rows(&self) -> Result<u16> {
        Ok(self.terminal.rows()?)
    }

    /// Scrollback rows above the live screen. The daemon publishes this
    /// so a client can clamp its own scroll range without asking the
    /// pane worker where the bottom is.
    pub fn scrollback_rows(&self) -> Result<usize> {
        Ok(self.terminal.scrollback_rows()?)
    }

    /// Erase every scrollback line (CSI 3 J) and, since real terminal
    /// semantics never touch the visible grid, also clear the visible
    /// screen of content that scrolled onto it just before the clear
    /// (plan Task 10). The prompt block - the shell's prompt and any
    /// in-progress typed command - survives and is left at the top of
    /// the screen, the position `clear` leaves a shell in.
    ///
    /// The block boundary is the top of the prompt block, not the
    /// cursor's row: a themed prompt (Tide) draws a decoration row above
    /// the input row the cursor sits on, and erasing up to the cursor
    /// took that decoration with it. The block height is measured before
    /// CSI 3 J runs, since the prompt rows are addressed in screen space
    /// (scrollback plus cursor row) and the clear rebases the screen
    /// onto an empty scrollback.
    ///
    /// The cursor's row comes from `cursor_x`/`cursor_y`, which are
    /// active-screen relative - the same frame CSI H addresses - so this
    /// is independent of where the viewport happens to be scrolled.
    pub fn clear_history(&mut self) {
        let keep_above = self.prompt_rows_above_live_cursor().unwrap_or(0);
        self.terminal.vt_write(b"\x1b[3J");
        if !self.terminal.is_cursor_visible().unwrap_or(false) {
            return;
        }
        let (Ok(col), Ok(row)) = (self.terminal.cursor_x(), self.terminal.cursor_y()) else {
            return;
        };
        // DELETE LINE from the top of the screen, not a row-by-row
        // erase: it takes the blank rows above the prompt block away
        // *and* scrolls the block up to row 1, which is where `clear`
        // leaves a shell - prompt at the top, clear space under it.
        // Every row shifts by the same amount, so the shell's model of
        // where its prompt is (an offset from the cursor) still holds
        // and its next repaint lands on the block instead of smearing
        // it. DELETE LINE leaves scrollback alone and resets the cursor
        // column, so the cursor is placed back on the block's last row
        // afterwards.
        let above = row.saturating_sub(keep_above);
        if above > 0 {
            self.terminal
                .vt_write(format!("\x1b[1;1H\x1b[{above}M").as_bytes());
        }
        self.terminal
            .vt_write(format!("\x1b[{};{}H", keep_above + 1, col + 1).as_bytes());
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
                let text: String = cell.graphemes()?.iter().collect();
                push_run(&mut line, &style, &text);
            }
            trim_trailing_default(&mut line);
            out.push(line);
        }
        Ok(out)
    }

    /// Rows `[offset, offset + rows)` in screen space as plain text plus
    /// styled runs, one `StyledLine` per row.
    ///
    /// Screen space is the whole grid: scrollback rows first, the visible
    /// screen last, so `offset` may point above the viewport. This is the
    /// walk `screen_lines` uses - `grid_ref` per cell with a
    /// `Point::Screen` - and it reads the grid without moving the
    /// emulator's own viewport. That is the point: one client's scroll is
    /// that client's view, and must not move another client's. The
    /// client's row-to-cell mapping (`text` line N is screen row
    /// `offset + N`) depends on it.
    ///
    /// `rows` is clamped to what the grid holds above and including
    /// `offset`: a client taller than the pane gets the rows that exist,
    /// not an error. The crate docs warn `grid_ref` is not built for
    /// render loops; this is one render per client scroll, not a frame.
    pub fn window_at(&mut self, offset: usize, rows: u16) -> Result<(String, Vec<StyledLine>)> {
        let cols = self.terminal.cols()?;
        let total = self.terminal.scrollback_rows()? + self.terminal.rows()? as usize;
        let rows = (rows as usize).min(total.saturating_sub(offset));
        let mut text = String::new();
        let mut out = Vec::with_capacity(rows);
        let mut graphemes = [0 as char; 8];
        for row in offset..offset + rows {
            let mut line = StyledLine { runs: Vec::new() };
            let mut plain = String::new();
            for col in 0..cols {
                let cell = self.terminal.grid_ref(Point::Screen(PointCoordinate {
                    x: col,
                    y: row as u32,
                }))?;
                let style = cell.style()?;
                let n = cell.graphemes(&mut graphemes)?;
                let text: String = graphemes[..n].iter().collect();
                plain.push_str(&text);
                push_run(&mut line, &style, &text);
            }
            trim_trailing_default(&mut line);
            text.push_str(plain.trim_end());
            text.push('\n');
            out.push(line);
        }
        Ok((text, out))
    }

    /// The pane's working directory as reported by OSC 7, decoded to a
    /// plain path. libghostty hands back the raw `file://host/path` URI, so
    /// the scheme and authority are stripped and percent-escapes decoded.
    /// Empty when the pane never reported one, or reported something that
    /// is not a `file://` URI.
    pub fn pwd(&mut self) -> Result<String> {
        Ok(pwd_path(self.terminal.pwd()?).unwrap_or_default())
    }
}

/// Strip the `file://` scheme and authority from an OSC 7 report, leaving
/// the path. Returns `None` for any other URI scheme.
fn pwd_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let at = rest.find('/')?;
    Some(percent_decode(&rest[at..]))
}

/// Decode `%XX` escapes, leaving anything malformed as literal text.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match (bytes[i], bytes.get(i + 1), bytes.get(i + 2)) {
            (b'%', Some(&hi), Some(&lo)) => match (hex_digit(hi), hex_digit(lo)) {
                (Some(hi), Some(lo)) => {
                    out.push(hi * 16 + lo);
                    i += 3;
                }
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            _ => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Append one cell to `line`, merging into the previous run when the
/// styling matches so uniform cells collapse into one run.
fn push_run(line: &mut StyledLine, style: &Style, text: &str) {
    let fg = cell_color(style.fg_color);
    let bg = cell_color(style.bg_color);
    let attrs = cell_attrs(style);
    match line.runs.last_mut() {
        Some(run) if run.fg == fg && run.bg == bg && run.attrs == attrs => {
            run.text.push_str(text);
        }
        _ => line.runs.push(StyledRun {
            text: text.to_string(),
            fg,
            bg,
            attrs,
        }),
    }
}

/// Drop trailing default-styled whitespace: it carries no information.
fn trim_trailing_default(line: &mut StyledLine) {
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
}

fn cell_attrs(style: &Style) -> CellAttrs {
    CellAttrs {
        bold: style.bold,
        italic: style.italic,
        underline: !matches!(style.underline, Underline::None),
        strikethrough: style.strikethrough,
        inverse: style.inverse,
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
    fn clear_history_moves_the_typed_command_to_the_top_row() {
        let mut emu = Emulator::new(80, 5).unwrap();
        for i in 1..=100 {
            emu.feed(format!("line{i}\r\n").as_bytes());
        }
        // A command typed but not yet submitted: the cursor sits on the
        // visible screen's last row, and that row must survive the clear,
        // at the top of the screen where `clear` leaves a shell.
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
            lines[0], "prompt$ cmd",
            "the typed command must land on the top row, got {lines:?}"
        );
        assert_eq!(cursor_row, 0, "the cursor rides up with its row");
        assert_eq!(cursor_col, 11, "cursor stays where typing left it");
        for row in lines.iter().skip(1) {
            assert!(
                row.is_empty(),
                "nothing else must survive the clear, got {lines:?}"
            );
        }
    }

    #[test]
    fn clear_history_keeps_the_themed_prompt_block_and_moves_it_to_the_top() {
        // Tide draws a two-row prompt: OSC 133 A marks a blank row, the
        // shell draws a decoration row (directory and time), and the
        // input row follows with OSC 133 B. Erasing every row above the
        // cursor's row took the decoration with it, so the visible
        // prompt lost the line the user reads it on.
        let mut emu = Emulator::new(80, 5).unwrap();
        for i in 1..=40 {
            emu.feed(format!("line{i}\r\n").as_bytes());
        }
        emu.feed(b"\x1b]133;A\x1b\\\r\n");
        emu.feed(b"~/dev 10:29\r\n");
        emu.feed(b"$ ");
        emu.feed(b"\x1b]133;B\x1b\\");
        emu.clear_history();
        assert_eq!(
            emu.terminal.scrollback_rows().unwrap(),
            0,
            "CSI 3 J must still empty the scrollback"
        );
        let text = emu.screen_text().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        // The block keeps its internal shape and sits at the top: the
        // marker's blank row first, then the decoration, then the input
        // row with the cursor on it.
        assert_eq!(
            lines[0], "",
            "the marker row leads the block, got {lines:?}"
        );
        assert_eq!(
            lines[1], "~/dev 10:29",
            "decoration row must survive under the marker, got {lines:?}"
        );
        assert!(
            lines[2].starts_with('$'),
            "input row must survive, got {lines:?}"
        );
        let (cursor_col, cursor_row) = emu.cursor().unwrap().expect("cursor visible");
        assert_eq!(
            cursor_row, 2,
            "the cursor moves with the block onto the input row"
        );
        assert_eq!(cursor_col, 2, "cursor stays where typing left it");
        for row in lines.iter().skip(3) {
            assert!(
                row.is_empty(),
                "nothing but the block must survive, got {lines:?}"
            );
        }
    }

    #[test]
    fn clear_history_empties_scrollback_and_the_screen_above_the_cursor() {
        // Contract narrowed by Task 10: the S3-6 acceptance criterion
        // ("active text is intact") only holds for the cursor's row and
        // below. Every line fed here ends in `\r\n`, so the cursor sits
        // on a blank row below "line100" - that row, and everything
        // above it, is now gone along with the scrollback.
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
    fn window_at_renders_any_row_range_without_moving_the_viewport() {
        // A per-client viewport means the daemon asks a worker for an
        // arbitrary window of screen-space rows instead of scrolling the
        // emulator. The window has to carry styling, because the client
        // has no other source for it, and it must leave the emulator's own
        // viewport exactly where it was, or one client's scroll would move
        // every other client's.
        let mut emu = Emulator::new(80, 5).unwrap();
        for i in 0..100 {
            emu.feed(format!("\x1b[31mline{i:03}\x1b[0m\r\n").as_bytes());
        }
        let before = emu.viewport_offset().unwrap();

        let (text, lines) = emu.window_at(10, 3).unwrap();
        assert_eq!(
            emu.viewport_offset().unwrap(),
            before,
            "window_at must not scroll the emulator"
        );
        assert_eq!(lines.len(), 3, "one line per requested row");
        let joined: Vec<String> = lines
            .iter()
            .map(|l| l.runs.iter().map(|r| r.text.as_str()).collect())
            .collect();
        assert_eq!(joined, vec!["line010", "line011", "line012"]);
        assert_eq!(text, "line010\nline011\nline012\n");
        for line in &lines {
            assert_eq!(
                line.runs.first().map(|r| r.fg),
                Some(CellColor::Indexed(1)),
                "each row carries its styling, got {line:?}"
            );
        }
    }

    #[test]
    fn window_at_the_live_offset_matches_screen_text() {
        // At the bottom the window render and the viewport render describe
        // the same rows, so they must agree row for row.
        let mut emu = Emulator::new(80, 5).unwrap();
        for i in 0..100 {
            emu.feed(format!("line{i:03}\r\n").as_bytes());
        }
        let offset = emu.viewport_offset().unwrap();
        let (text, _) = emu.window_at(offset, 5).unwrap();
        assert_eq!(text, emu.screen_text().unwrap());
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

    #[test]
    fn a_pane_that_never_reported_a_pwd_returns_empty() {
        let mut emu = Emulator::new(80, 24).unwrap();
        assert_eq!(emu.pwd().unwrap(), "");
    }

    #[test]
    fn an_osc_7_sequence_sets_the_pwd_without_the_uri_parts() {
        let mut emu = Emulator::new(80, 24).unwrap();
        emu.feed(b"\x1b]7;file://localhost/tmp/project\x1b\\");
        assert_eq!(emu.pwd().unwrap(), "/tmp/project");
    }

    #[test]
    fn a_later_pwd_replaces_the_earlier_one() {
        let mut emu = Emulator::new(80, 24).unwrap();
        emu.feed(b"\x1b]7;file://localhost/tmp/one\x1b\\");
        emu.feed(b"\x1b]7;file://localhost/tmp/two\x1b\\");
        assert_eq!(emu.pwd().unwrap(), "/tmp/two");
    }

    #[test]
    fn a_pwd_with_escapes_is_decoded() {
        let mut emu = Emulator::new(80, 24).unwrap();
        emu.feed(b"\x1b]7;file://localhost/tmp/with%20space\x1b\\");
        assert_eq!(emu.pwd().unwrap(), "/tmp/with space");
    }

    #[test]
    fn a_pwd_report_without_a_host_is_still_a_path() {
        let mut emu = Emulator::new(80, 24).unwrap();
        emu.feed(b"\x1b]7;file:///tmp/bare\x1b\\");
        assert_eq!(emu.pwd().unwrap(), "/tmp/bare");
    }

    #[test]
    fn a_non_file_uri_reports_no_pwd() {
        let mut emu = Emulator::new(80, 24).unwrap();
        emu.feed(b"\x1b]7;ssh://host/tmp/project\x1b\\");
        assert_eq!(emu.pwd().unwrap(), "");
    }

    #[test]
    fn a_truncated_escape_is_left_literal() {
        let mut emu = Emulator::new(80, 24).unwrap();
        emu.feed(b"\x1b]7;file://localhost/tmp/a%2\x1b\\");
        assert_eq!(emu.pwd().unwrap(), "/tmp/a%2");
    }
}
