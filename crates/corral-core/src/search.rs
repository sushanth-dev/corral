//! Scrollback search over the terminal's screen-space grid. Plan Task
//! S3-5. `Emulator::search` walks rows in `PointSpace::Screen`
//! coordinates (0 = first scrollback row) via `Terminal::grid_ref`,
//! which libghostty documents as the non-render-loop path for exactly
//! this kind of one-shot lookup. Case-sensitive for v0.2.

use anyhow::Result;
use libghostty_vt::terminal::{Point, PointCoordinate};

use crate::emulation::Emulator;

impl Emulator {
    /// Rows (screen space, 0-indexed from the top of scrollback) whose
    /// text contains `needle`. Starts after `from` when given, or from
    /// the bottom when `reverse` is set, so repeated searches resume
    /// where the last one landed.
    pub fn search(
        &mut self,
        needle: &str,
        from: Option<usize>,
        reverse: bool,
    ) -> Result<Vec<usize>> {
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        // Screen space spans history plus the active screen.
        let total = self.terminal.scrollback_rows()? + self.terminal.rows()? as usize;
        if total == 0 {
            return Ok(Vec::new());
        }
        let start = match (from, reverse) {
            (Some(f), false) => f + 1,
            (Some(f), true) => f.saturating_sub(1),
            (None, false) => 0,
            (None, true) => total - 1,
        };
        let range: Box<dyn Iterator<Item = usize>> = if reverse {
            Box::new((0..=start).rev())
        } else {
            Box::new(start..total)
        };
        let mut hits = Vec::new();
        for row in range {
            let text = self.row_text(row)?;
            if text.contains(needle) {
                hits.push(row);
            }
        }
        Ok(hits)
    }

    /// The full scrollback plus active screen as plain text, one
    /// screen-space row per line (S3-7). This is what the client writes
    /// to the temp file for $EDITOR.
    pub fn dump_scrollback(&mut self) -> Result<String> {
        let total = self.terminal.scrollback_rows()? + self.terminal.rows()? as usize;
        let mut out = String::new();
        for row in 0..total {
            out.push_str(&self.row_text(row)?);
            out.push('\n');
        }
        Ok(out)
    }

    /// One screen-space row as plain text. Each cell resolves through
    /// `grid_ref`; one-shot operations (search, dump) accept the
    /// documented cost of screen-space lookups.
    pub(crate) fn row_text(&mut self, row: usize) -> Result<String> {
        let cols = self.terminal.cols()?;
        let mut line = String::new();
        let mut buf = [0 as char; 8];
        for col in 0..cols {
            let grid = self.terminal.grid_ref(Point::Screen(PointCoordinate {
                x: col,
                y: row as u32,
            }))?;
            let n = grid.graphemes(&mut buf)?;
            line.extend(&buf[..n]);
        }
        Ok(line.trim_end().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulation::ScrollTarget;

    fn marker_emulator() -> Emulator {
        // 50 lines, every tenth carrying a distinctive marker. The
        // emulator is 80x24, so the first 24 lines scroll into history.
        let mut emu = Emulator::new(80, 24).unwrap();
        for i in 0..50 {
            if i % 10 == 0 {
                emu.feed(format!("NEEDLEMARK line {i}\r\n").as_bytes());
            } else {
                emu.feed(format!("plain line {i}\r\n").as_bytes());
            }
        }
        emu
    }

    #[test]
    fn search_finds_rows_in_screen_space() {
        let mut emu = marker_emulator();
        let hits = emu.search("NEEDLEMARK", None, false).unwrap();
        assert_eq!(hits, vec![0, 10, 20, 30, 40], "markers sit on those rows");
    }

    #[test]
    fn search_from_resumes_after_the_offset() {
        let mut emu = marker_emulator();
        let hits = emu.search("NEEDLEMARK", Some(10), false).unwrap();
        assert_eq!(hits, vec![20, 30, 40], "rows at or before 10 are skipped");
    }

    #[test]
    fn reverse_search_walks_up_from_the_bottom() {
        let mut emu = marker_emulator();
        // From the bottom (no `from`), the first reverse hits are the
        // deepest markers.
        let hits = emu.search("NEEDLEMARK", None, true).unwrap();
        assert_eq!(hits, vec![40, 30, 20, 10, 0]);
        // Resume above row 40.
        let next = emu.search("NEEDLEMARK", Some(40), true).unwrap();
        assert_eq!(next, vec![30, 20, 10, 0]);
    }

    #[test]
    fn search_misses_return_empty() {
        let mut emu = marker_emulator();
        assert!(emu.search("absent-token", None, false).unwrap().is_empty());
    }

    #[test]
    fn empty_needle_searches_nothing() {
        let mut emu = marker_emulator();
        assert!(emu.search("", None, false).unwrap().is_empty());
    }

    #[test]
    fn search_is_case_sensitive() {
        let mut emu = marker_emulator();
        assert!(emu.search("needlemark", None, false).unwrap().is_empty());
    }

    #[test]
    fn search_sees_scrolled_back_content() {
        let mut emu = marker_emulator();
        // Viewport shows only the last rows; the search still reaches
        // the top of scrollback.
        emu.scroll(ScrollTarget::Top);
        let hits = emu.search("NEEDLEMARK", None, false).unwrap();
        assert_eq!(hits.first().copied(), Some(0));
    }

    #[test]
    fn dump_scrollback_returns_every_screen_space_row() {
        // 50 lines into an 80x24 emulator: history plus active screen,
        // all in order. The grid also carries blank rows (the cursor
        // line below the last feed), so compare the non-empty lines.
        let mut emu = marker_emulator();
        let dump = emu.dump_scrollback().unwrap();
        let lines: Vec<&str> = dump.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 50, "every fed line is in the dump");
        assert!(
            lines.first().unwrap().contains("NEEDLEMARK line 0"),
            "top of scrollback first, got {:?}",
            lines.first().unwrap()
        );
        assert!(
            lines.last().unwrap().contains("plain line 49"),
            "active screen last, got {:?}",
            lines.last().unwrap()
        );
    }

    #[test]
    fn dump_scrollback_of_a_fresh_screen_has_no_content() {
        let mut emu = Emulator::new(80, 24).unwrap();
        assert!(
            emu.dump_scrollback()
                .unwrap()
                .lines()
                .all(|l| l.trim().is_empty()),
            "fresh screen dumps blank rows only"
        );
    }
}
