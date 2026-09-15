//! Scrollback search over the terminal's screen-space grid. Plan Task
//! S3-5. `Emulator::search` walks rows in `PointSpace::Screen`
//! coordinates (0 = first scrollback row) via `Terminal::grid_ref`,
//! which libghostty documents as the non-render-loop path for exactly
//! this kind of one-shot lookup. Case-sensitive for v0.2.

use anyhow::Result;
use libghostty_vt::screen::CellSemanticContent;
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

    /// Screen-space rows whose semantic prompt state is `Prompt` (S3-8).
    /// Shell-integrated shells emit OSC 133 A at each prompt; rows
    /// without markers report `None` and yield no prompt rows.
    ///
    /// The marker row is usually blank: fish and zsh print OSC 133 A
    /// before drawing the prompt, so the shell's prompt text lives on
    /// the marker row or the one below. `prompt_rows` reports the
    /// marker row; callers that need the visible prompt use
    /// `prompt_text_rows`, which shifts a marker to the next
    /// non-empty row when the marker row itself is blank.
    pub fn prompt_rows(&mut self) -> Result<Vec<usize>> {
        let total = self.terminal.scrollback_rows()? + self.terminal.rows()? as usize;
        let mut rows = Vec::new();
        for row in 0..total {
            let grid = self.terminal.grid_ref(Point::Screen(PointCoordinate {
                x: 0,
                y: row as u32,
            }))?;
            let prompt = grid.row()?.semantic_prompt()?;
            if matches!(prompt, libghostty_vt::screen::RowSemanticPrompt::Prompt) {
                rows.push(row);
            }
        }
        Ok(rows)
    }

    /// Prompt rows with blank marker rows shifted to the visible
    /// prompt: a marker on an empty row maps to the next non-empty row
    /// (the shell's prompt text); markers already on text map to
    /// themselves. Rows resolve through `row_text`, so an all-blank
    /// tail keeps its marker as-is.
    pub fn prompt_text_rows(&mut self) -> Result<Vec<usize>> {
        let total = self.terminal.scrollback_rows()? + self.terminal.rows()? as usize;
        let mut out = Vec::new();
        for row in self.prompt_rows()? {
            let mut mapped = row;
            for r in row..total {
                if !self.row_text(r)?.is_empty() {
                    mapped = r;
                    break;
                }
            }
            out.push(mapped);
        }
        out.dedup();
        Ok(out)
    }

    /// Screen-space (row, col) of each prompt's command input, from OSC
    /// 133;B. fish and zsh mark the exact cell where typed input starts
    /// with `CellSemanticContent::Input`; a shell theme (Tide) can wrap
    /// the prompt in a multi-row box, so scanning cell-by-cell from each
    /// OSC 133;A marker to the next finds the real command position
    /// instead of the marker row or the box-drawing glyphs.
    pub fn prompt_input_positions(&mut self) -> Result<Vec<(usize, u16)>> {
        let total = self.terminal.scrollback_rows()? + self.terminal.rows()? as usize;
        let cols = self.terminal.cols()?;
        let markers = self.prompt_rows()?;
        let mut out = Vec::with_capacity(markers.len());
        for (i, &start) in markers.iter().enumerate() {
            let end = markers.get(i + 1).copied().unwrap_or(total);
            let mut input_cell = None;
            'scan: for row in start..end {
                for col in 0..cols {
                    let grid = self.terminal.grid_ref(Point::Screen(PointCoordinate {
                        x: col,
                        y: row as u32,
                    }))?;
                    if grid.cell()?.semantic_content()? == CellSemanticContent::Input {
                        input_cell = Some((row, col));
                        break 'scan;
                    }
                }
            }
            let pos = match input_cell {
                Some(p) => p,
                None => {
                    // No OSC 133;B seen: fall back to the first
                    // non-empty row (prompt_text_rows' heuristic), left
                    // column.
                    let mut mapped = (start, 0);
                    for r in start..end {
                        if !self.row_text(r)?.is_empty() {
                            mapped = (r, 0);
                            break;
                        }
                    }
                    mapped
                }
            };
            out.push(pos);
        }
        Ok(out)
    }

    /// The text of the command whose prompt is the last one at or above
    /// `anchor`: prompt row through the row before the next prompt
    /// (S3-8). `anchor` is a screen-space row; `None` means the bottom
    /// of scrollback. Prompt rows resolve through
    /// `prompt_input_positions` so the block starts at the command's
    /// real input row, not a decorative wrapper row a themed prompt
    /// (Tide) draws above it.
    pub fn command_text(&mut self, anchor: Option<usize>) -> Result<String> {
        let total = self.terminal.scrollback_rows()? + self.terminal.rows()? as usize;
        let anchor = anchor.unwrap_or(total.saturating_sub(1));
        let prompts: Vec<usize> = self
            .prompt_input_positions()?
            .into_iter()
            .map(|(row, _)| row)
            .collect();
        let start = prompts.iter().rev().find(|&&r| r <= anchor).copied();
        let Some(start) = start else {
            return Ok(String::new());
        };
        let end = prompts
            .iter()
            .find(|&&r| r > start)
            .copied()
            .unwrap_or(total);
        let mut out = String::new();
        for row in start..end {
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

    #[test]
    fn prompt_rows_finds_osc133_markers() {
        // The test shell does not emit OSC133; the markers arrive
        // literally (plan Task S3-8 guardrail). Three prompt rows in a
        // 50-line feed.
        let mut emu = Emulator::new(80, 24).unwrap();
        for i in 0..3 {
            emu.feed(b"\x1b]133;A\x1b\\");
            emu.feed(format!("prompt {i}\r\n").as_bytes());
            for j in 0..14 {
                emu.feed(format!("output {i}.{j}\r\n").as_bytes());
            }
        }
        let rows = emu.prompt_rows().unwrap();
        assert_eq!(rows, vec![0, 15, 30], "markers land on those rows");
    }

    #[test]
    fn prompt_text_rows_shifts_blank_markers_to_the_prompt() {
        // fish and zsh emit the OSC133 A marker on the blank line
        // above the drawn prompt, so the raw marker row is not where
        // the user sees the prompt. prompt_text_rows moves each blank
        // marker down to the first non-empty row below it.
        let mut emu = Emulator::new(80, 24).unwrap();
        for i in 0..3 {
            // Marker row is blank: the marker sits on its own line and
            // the prompt text follows on the next row, like fish does.
            emu.feed(b"\x1b]133;A\x1b\\\r\n");
            emu.feed(format!("$ prompt {i}\r\n").as_bytes());
            for j in 0..14 {
                emu.feed(format!("output {i}.{j}\r\n").as_bytes());
            }
        }
        let rows = emu.prompt_text_rows().unwrap();
        assert_eq!(rows, vec![1, 17, 33], "each marker shifted to its text row");
    }

    #[test]
    fn prompt_input_positions_lands_on_the_command_not_the_wrapper() {
        // A Tide-style two-row prompt: OSC 133;A marks a blank row, a
        // decorative wrapper line follows, and OSC 133;B marks the exact
        // cell (row, col) where the command text starts on the third
        // row. prompt_input_positions must resolve to that cell, not
        // the marker row or the wrapper row.
        let mut emu = Emulator::new(80, 24).unwrap();
        for i in 0..3 {
            emu.feed(b"\x1b]133;A\x1b\\\r\n");
            emu.feed(b"almost-a-prompt-wrapper\r\n");
            emu.feed(b"$ ");
            emu.feed(b"\x1b]133;B\x1b\\");
            emu.feed(format!("cmd {i}\r\n").as_bytes());
            for j in 0..5 {
                emu.feed(format!("out {i}.{j}\r\n").as_bytes());
            }
        }
        let positions = emu.prompt_input_positions().unwrap();
        assert_eq!(
            positions,
            vec![(2, 2), (10, 2), (18, 2)],
            "each command starts two rows below its marker, at column 2"
        );
    }

    #[test]
    fn prompt_input_positions_without_osc133b_falls_back_to_the_text_row() {
        // No OSC 133;B: fall back to the first non-empty row, column 0,
        // same as prompt_text_rows.
        let mut emu = Emulator::new(80, 24).unwrap();
        emu.feed(b"\x1b]133;A\x1b\\\r\n");
        emu.feed(b"$ plain prompt\r\n");
        let positions = emu.prompt_input_positions().unwrap();
        assert_eq!(positions, vec![(1, 0)]);
    }

    #[test]
    fn prompt_rows_without_markers_is_empty() {
        let mut emu = marker_emulator();
        assert!(
            emu.prompt_rows().unwrap().is_empty(),
            "plain output yields no prompt rows"
        );
    }

    #[test]
    fn command_text_returns_prompt_through_last_output_row() {
        let mut emu = Emulator::new(80, 24).unwrap();
        for i in 0..3 {
            emu.feed(b"\x1b]133;A\x1b\\");
            emu.feed(format!("cmd {i}\r\n").as_bytes());
            for j in 0..5 {
                emu.feed(format!("out {i}-{j}\r\n").as_bytes());
            }
        }
        // Anchor at the bottom: the third command's block.
        let text = emu.command_text(None).unwrap();
        assert!(text.contains("cmd 2"), "got {text:?}");
        assert!(text.contains("out 2-4"), "got {text:?}");
        assert!(
            !text.contains("out 1-"),
            "block must stop at the next prompt, got {text:?}"
        );
        // Anchor inside the first block: cmd 0's output only.
        let first = emu.command_text(Some(3)).unwrap();
        assert!(first.contains("cmd 0"), "got {first:?}");
        assert!(first.contains("out 0-4"), "got {first:?}");
        assert!(!first.contains("cmd 1"), "got {first:?}");
    }

    #[test]
    fn command_text_excludes_the_wrapper_row_on_themed_prompts() {
        // Same Tide-style layout as
        // prompt_input_positions_lands_on_the_command_not_the_wrapper:
        // command_text must start at the resolved input row, not the
        // decorative wrapper line under the marker.
        let mut emu = Emulator::new(80, 24).unwrap();
        for i in 0..2 {
            emu.feed(b"\x1b]133;A\x1b\\\r\n");
            emu.feed(b"almost-a-prompt-wrapper\r\n");
            emu.feed(b"$ ");
            emu.feed(b"\x1b]133;B\x1b\\");
            emu.feed(format!("cmd {i}\r\n").as_bytes());
            for j in 0..5 {
                emu.feed(format!("out {i}.{j}\r\n").as_bytes());
            }
        }
        // Anchor at the bottom: the last command's block, so there is
        // no next prompt to muddy the end boundary.
        let text = emu.command_text(None).unwrap();
        assert!(text.contains("cmd 1"), "got {text:?}");
        assert!(text.contains("out 1.4"), "got {text:?}");
        assert!(
            !text.contains("almost-a-prompt-wrapper"),
            "block must start at the command, not the wrapper row, got {text:?}"
        );
    }

    #[test]
    fn command_text_without_prompts_is_empty() {
        let mut emu = marker_emulator();
        assert_eq!(emu.command_text(None).unwrap(), "");
    }
}
