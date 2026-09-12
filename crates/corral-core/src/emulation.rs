use anyhow::Result;
use libghostty_vt::render::{CellIterator, RowIterator};
use libghostty_vt::{RenderState, Terminal, TerminalOptions};

/// One pane's terminal state. `Terminal` is !Send: Emulator must be
/// created and used on a single thread (the daemon core thread in v0.1).
pub struct Emulator {
    terminal: Terminal<'static, 'static>,
}

impl Emulator {
    pub fn new(cols: u16, rows: u16) -> Result<Self> {
        let opts = TerminalOptions {
            cols,
            rows,
            max_scrollback: 10_000,
        };
        Ok(Self {
            terminal: Terminal::new(opts)?,
        })
    }

    /// Bytes straight from the PTY.
    pub fn feed(&mut self, data: &[u8]) {
        self.terminal.vt_write(data);
    }

    /// Resize the viewport; libghostty reflows the primary screen.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.terminal.resize(cols, rows, 0, 0)?;
        Ok(())
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
}
