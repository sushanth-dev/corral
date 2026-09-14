//! Copy-mode text selection over the client's rendered text grid.
//! Coordinates are (row, col) into the focused pane's `PaneState.text`.
//! Plain text only; the recorded simplification in the plan (no styled
//! yank, no libghostty Selection).

/// Selection shape. `v` starts a spanning selection (partial first and
/// last rows); `V` starts a rectangle selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectMode {
    Span,
    Rect,
}

/// An active selection. The anchor is where `v` was pressed; `cursor`
/// moves with vi motions and the range between them highlights and yanks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub mode: SelectMode,
    pub anchor: (usize, usize),
    pub cursor: (usize, usize),
}

/// Grid geometry of the focused pane's text: line lengths and count.
pub struct Grid {
    pub rows: Vec<usize>,
}

impl Grid {
    pub fn from_text(text: &str) -> Self {
        Self {
            rows: text.lines().map(|l| l.chars().count()).collect(),
        }
    }

    pub fn height(&self) -> usize {
        self.rows.len()
    }

    fn clamp(&self, pos: (usize, usize)) -> (usize, usize) {
        if self.rows.is_empty() {
            return (0, 0);
        }
        let row = pos.0.min(self.rows.len() - 1);
        // Cursor can sit one past the last char (end of line).
        let col = pos.1.min(self.rows[row]);
        (row, col)
    }
}

impl Selection {
    pub fn start(mode: SelectMode, anchor: (usize, usize)) -> Self {
        Self {
            mode,
            anchor,
            cursor: anchor,
        }
    }

    /// Move the cursor by (drow, dcol), clamped to the grid.
    pub fn extend(&mut self, grid: &Grid, drow: isize, dcol: isize) {
        let (r, c) = self.cursor;
        let target = (r as isize + drow, c as isize + dcol);
        let row = target.0.clamp(0, grid.height() as isize - 1).max(0) as usize;
        let col = target
            .1
            .clamp(0, grid.rows.get(row).copied().unwrap_or(0) as isize)
            .max(0) as usize;
        self.cursor = (row, col);
    }

    /// Normalized (start, end) with start <= end in row-major order.
    /// End is inclusive of the cursor cell.
    pub fn range(&self, grid: &Grid) -> ((usize, usize), (usize, usize)) {
        let a = grid.clamp(self.anchor);
        let c = grid.clamp(self.cursor);
        if (a.0, a.1) <= (c.0, c.1) {
            (a, c)
        } else {
            (c, a)
        }
    }

    /// Highlight spans per row: (row, first col, last col inclusive).
    /// Span mode runs from the anchor col to the line end on the first
    /// row and from col 0 on the last row; rect mode uses the anchor
    /// and cursor columns on every row.
    pub fn spans(&self, text: &str) -> Vec<(usize, usize, usize)> {
        let grid = Grid::from_text(text);
        if grid.height() == 0 {
            return Vec::new();
        }
        let ((r0, c0), (r1, c1)) = self.range(&grid);
        (r0..=r1)
            .map(|row| {
                let len = grid.rows[row];
                let (first, last) = match self.mode {
                    SelectMode::Span => {
                        if row == r0 && row == r1 {
                            (c0, c1)
                        } else if row == r0 {
                            (c0, len.saturating_sub(1))
                        } else if row == r1 {
                            (0, c1)
                        } else {
                            (0, len.saturating_sub(1))
                        }
                    }
                    SelectMode::Rect => {
                        let lo = c0.min(c1);
                        let hi = c0.max(c1);
                        (lo.min(len.saturating_sub(1)), hi.min(len.saturating_sub(1)))
                    }
                };
                (row, first, last)
            })
            .collect()
    }

    /// The selected text: char span per row in span mode, a fixed
    /// column window padded with spaces in rect mode.
    pub fn text(&self, text: &str) -> String {
        let grid = Grid::from_text(text);
        if grid.height() == 0 {
            return String::new();
        }
        let ((r0, c0), (r1, c1)) = self.range(&grid);
        let lines: Vec<&str> = text.lines().collect();
        if self.mode == SelectMode::Rect {
            let (lo, hi) = (c0.min(c1), c0.max(c1));
            let mut out = String::new();
            for row in r0..=r1 {
                if row > r0 {
                    out.push('\n');
                }
                let chars: Vec<char> = lines.get(row).copied().unwrap_or("").chars().collect();
                for col in lo..=hi {
                    out.push(chars.get(col).copied().unwrap_or(' '));
                }
            }
            return out;
        }
        if r0 == r1 {
            let line = lines.get(r0).copied().unwrap_or("");
            let chars: Vec<char> = line.chars().collect();
            let end = (c1 + 1).min(chars.len());
            chars[c0.min(end)..end].iter().collect()
        } else {
            let mut out = String::new();
            for (i, line) in lines.iter().enumerate().take(r1 + 1).skip(r0) {
                if i > r0 {
                    out.push('\n');
                }
                let chars: Vec<char> = line.chars().collect();
                if i == r0 {
                    out.push_str(&chars[c0.min(chars.len())..].iter().collect::<String>());
                } else if i == r1 {
                    let end = (c1 + 1).min(chars.len());
                    out.push_str(&chars[..end].iter().collect::<String>());
                } else {
                    out.push_str(line);
                }
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "alpha\nbeta\ngamma\ndelta\n";

    #[test]
    fn start_places_anchor_and_cursor_together() {
        let sel = Selection::start(SelectMode::Span, (1, 2));
        assert_eq!(sel.anchor, (1, 2));
        assert_eq!(sel.cursor, (1, 2));
        assert_eq!(sel.text(TEXT), "t");
    }

    #[test]
    fn motion_extends_and_yanks_the_range() {
        // From 'b' in beta, j j and two rights land mid-gamma.
        let mut sel = Selection::start(SelectMode::Span, (1, 0));
        let grid = Grid::from_text(TEXT);
        sel.extend(&grid, 2, 2);
        assert_eq!(sel.cursor, (3, 2));
        assert_eq!(sel.text(TEXT), "beta\ngamma\ndel");
    }

    #[test]
    fn upward_motion_flips_the_range() {
        let mut sel = Selection::start(SelectMode::Span, (3, 1));
        let grid = Grid::from_text(TEXT);
        sel.extend(&grid, -3, 0);
        assert_eq!(sel.cursor, (0, 1));
        // Row 0 from col 1, rows 1-2 whole, row 3 up to and including col 1.
        assert_eq!(sel.text(TEXT), "lpha\nbeta\ngamma\nde");
    }

    #[test]
    fn motion_clamps_to_the_grid() {
        let mut sel = Selection::start(SelectMode::Span, (0, 0));
        let grid = Grid::from_text(TEXT);
        sel.extend(&grid, -10, -10);
        assert_eq!(sel.cursor, (0, 0));
        sel.extend(&grid, 100, 100);
        assert_eq!(
            sel.cursor,
            (3, 5),
            "delta is 5 chars; cursor may sit one past"
        );
        assert_eq!(sel.text(TEXT), "alpha\nbeta\ngamma\ndelta");
    }

    #[test]
    fn selection_on_one_row_is_the_char_span() {
        let mut sel = Selection::start(SelectMode::Span, (2, 1));
        let grid = Grid::from_text(TEXT);
        sel.extend(&grid, 0, 2);
        assert_eq!(sel.text(TEXT), "amm");
    }

    #[test]
    fn empty_grid_yanks_nothing() {
        let sel = Selection::start(SelectMode::Span, (0, 0));
        assert_eq!(sel.text(""), "");
    }

    #[test]
    fn wide_graphemes_select_by_char_not_byte() {
        let text = "こんにちは\nworld\n";
        let mut sel = Selection::start(SelectMode::Span, (0, 0));
        let grid = Grid::from_text(text);
        sel.extend(&grid, 0, 1);
        assert_eq!(sel.text(text), "こん");
    }

    #[test]
    fn range_is_normalized_for_highlighting() {
        let mut sel = Selection::start(SelectMode::Span, (2, 3));
        let grid = Grid::from_text(TEXT);
        sel.extend(&grid, -1, -1);
        let ((r0, c0), (r1, c1)) = sel.range(&grid);
        assert_eq!((r0, c0), (1, 2));
        assert_eq!((r1, c1), (2, 3));
    }

    #[test]
    fn rect_yank_takes_a_fixed_column_window() {
        // Columns 2..=3 of beta and gamma; alpha is longer so the
        // window exists, and shorter lines would pad.
        let mut sel = Selection::start(SelectMode::Rect, (1, 2));
        let grid = Grid::from_text(TEXT);
        sel.extend(&grid, 1, 1);
        assert_eq!(sel.text(TEXT), "ta\nmm");
    }

    #[test]
    fn rect_yank_pads_short_lines_with_spaces() {
        let text = "ab\ndefgh\n";
        let mut sel = Selection::start(SelectMode::Rect, (0, 1));
        let grid = Grid::from_text(text);
        sel.extend(&grid, 1, 2);
        assert_eq!(sel.text(text), "b  \nefg");
    }

    #[test]
    fn spans_cover_first_row_to_the_line_end() {
        let mut sel = Selection::start(SelectMode::Span, (1, 2));
        let grid = Grid::from_text(TEXT);
        sel.extend(&grid, 1, 0);
        assert_eq!(sel.spans(TEXT), vec![(1, 2, 3), (2, 0, 2)]);
    }

    #[test]
    fn rect_spans_use_min_and_max_columns() {
        let mut sel = Selection::start(SelectMode::Rect, (2, 3));
        let grid = Grid::from_text(TEXT);
        sel.extend(&grid, -1, -2);
        // Range flips to rows 1-2, cols 1..=3 on both rows.
        assert_eq!(sel.spans(TEXT), vec![(1, 1, 3), (2, 1, 3)]);
    }
}
