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
        assert_eq!(text.lines().nth(0).unwrap(), "abcdefghij");
        assert!(text.lines().nth(1).unwrap().starts_with("klmnop"));
    }
}
