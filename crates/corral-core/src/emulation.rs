use anyhow::Result;
use libghostty_vt::render::{CellIterator, RowIterator};
use libghostty_vt::terminal::{
    ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType,
    PrimaryDeviceAttributes, SecondaryDeviceAttributes, TertiaryDeviceAttributes,
};
use libghostty_vt::{RenderState, Terminal, TerminalOptions};
use std::cell::RefCell;
use std::rc::Rc;

/// One pane's terminal state. `Terminal` is !Send: Emulator must be
/// created and used on a single thread (the daemon core thread in v0.1).
pub struct Emulator {
    terminal: Terminal<'static, 'static>,
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
}
