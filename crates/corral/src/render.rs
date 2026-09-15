use crate::theme::Theme;
use corral_core::emulation::CellColor;
use corral_core::tree::PaneId;
use corrald::protocol::PaneState;
use ratatui::Frame;
use ratatui::layout::Rect as RRect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// Selection highlight spans for one pane: (row, first col, last col
/// inclusive) in text-grid coordinates.
pub type SpanList = Vec<(usize, usize, usize)>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hint {
    None,
    /// Copy mode active; carries the focused pane's viewport position
    /// (`None` while pinned to the bottom).
    Copy(Option<(usize, usize)>),
    /// Copy mode with an active selection.
    Select,
    /// Search prompt active; carries the needle typed so far.
    Search(String),
}

#[allow(clippy::too_many_arguments)]
pub fn draw(
    frame: &mut Frame,
    panes: &[PaneState],
    focused: PaneId,
    hint: Hint,
    spans: &[(PaneId, SpanList)],
    search: &[(PaneId, SpanList)],
    current: &[(PaneId, SpanList)],
    cursor: Option<(usize, usize)>,
    theme: &Theme,
) {
    for pane in panes {
        let rr = RRect {
            x: pane.rect.x,
            y: pane.rect.y,
            width: pane.rect.w,
            height: pane.rect.h,
        };
        let lines = pane_lines(pane);
        // No padding: the tree's 1-cell gutter is the whole separator.
        let para = Paragraph::new(lines);
        frame.render_widget(para, rr);
    }
    for (pane_id, sel_spans) in spans {
        let Some(p) = panes.iter().find(|p| p.id == *pane_id) else {
            continue;
        };
        paint_spans(
            frame,
            p,
            sel_spans,
            Style::new().add_modifier(Modifier::REVERSED),
        );
    }
    // Search hits paint yellow-on-black so they read at a glance.
    for (pane_id, hit_spans) in search {
        let Some(p) = panes.iter().find(|p| p.id == *pane_id) else {
            continue;
        };
        paint_spans(
            frame,
            p,
            hit_spans,
            Style::new()
                .fg(theme.palette.search_fg)
                .bg(theme.palette.search_bg),
        );
    }
    // The current match paints over the search style with its own color
    // so the user can tell which hit the cursor is on.
    for (pane_id, hit_spans) in current {
        let Some(p) = panes.iter().find(|p| p.id == *pane_id) else {
            continue;
        };
        paint_spans(
            frame,
            p,
            hit_spans,
            Style::new()
                .fg(theme.palette.current_hit_fg)
                .bg(theme.palette.current_hit_bg),
        );
    }
    if let Some(p) = panes.iter().find(|p| p.id == focused) {
        paint_cursor(frame, p, cursor, theme);
    }
    paint_gutters(frame, panes, focused, theme);
    draw_hint(frame, hint, theme);
}

/// The pane's visible rows as styled ratatui lines. Falls back to plain
/// `text` when the daemon sent no style runs (tests, benchmarks).
fn pane_lines(pane: &PaneState) -> Vec<Line<'static>> {
    if !pane.lines.is_empty() {
        return pane
            .lines
            .iter()
            .map(|line| {
                Line::from(
                    line.runs
                        .iter()
                        .map(|run| Span::styled(run.text.clone(), run_style(run)))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
    }
    pane.text
        .lines()
        .map(|l| Line::from(l.to_string()))
        .collect()
}

fn run_style(run: &corral_core::emulation::StyledRun) -> Style {
    let mut style = Style::new();
    style = match run.fg {
        CellColor::Default => style,
        CellColor::Indexed(i) => style.fg(Color::Indexed(i)),
        CellColor::Rgb(r, g, b) => style.fg(Color::Rgb(r, g, b)),
    };
    style = match run.bg {
        CellColor::Default => style,
        CellColor::Indexed(i) => style.bg(Color::Indexed(i)),
        CellColor::Rgb(r, g, b) => style.bg(Color::Rgb(r, g, b)),
    };
    let a = run.attrs;
    let mut mods = Modifier::empty();
    if a.bold {
        mods |= Modifier::BOLD;
    }
    if a.italic {
        mods |= Modifier::ITALIC;
    }
    if a.underline {
        mods |= Modifier::UNDERLINED;
    }
    if a.strikethrough {
        mods |= Modifier::CROSSED_OUT;
    }
    if a.inverse {
        mods |= Modifier::REVERSED;
    }
    style.add_modifier(mods)
}

// The copy-mode cursor paints one viewport cell. The pane rect maps
// the viewport coordinates to screen cells.
fn paint_cursor(
    frame: &mut Frame,
    pane: &PaneState,
    cursor: Option<(usize, usize)>,
    theme: &Theme,
) {
    let Some((row, col)) = cursor else {
        return;
    };
    let y = pane.rect.y + row as u16;
    let x = pane.rect.x + col as u16;
    if y >= pane.rect.y + pane.rect.h || x >= pane.rect.x + pane.rect.w {
        return;
    }
    let cell = &mut frame.buffer_mut()[(x, y)];
    cell.set_bg(theme.palette.cursor_bg);
    cell.set_fg(theme.palette.cursor_fg);
}

/// Every occurrence of `needle` in `text` as highlight spans: one
/// (row, first col, last col inclusive) per match. Empty needles
/// match nothing.
pub fn search_spans(text: &str, needle: &str) -> SpanList {
    if needle.is_empty() {
        return Vec::new();
    }
    let needle_chars: Vec<char> = needle.chars().collect();
    let mut spans = Vec::new();
    for (row, line) in text.lines().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        let mut col = 0;
        while col + needle_chars.len() <= chars.len() {
            if chars[col..col + needle_chars.len()] == needle_chars[..] {
                spans.push((row, col, col + needle_chars.len() - 1));
                col += needle_chars.len();
            } else {
                col += 1;
            }
        }
    }
    spans
}

/// The current match's span: occurrences of `needle` on `row` only,
/// painted by the caller with the distinct current-hit style.
pub fn current_hit_spans(text: &str, needle: &str, row: usize) -> SpanList {
    if needle.is_empty() {
        return Vec::new();
    }
    let Some(line) = text.lines().nth(row) else {
        return Vec::new();
    };
    let needle_chars: Vec<char> = needle.chars().collect();
    let chars: Vec<char> = line.chars().collect();
    let mut spans = Vec::new();
    let mut col = 0;
    while col + needle_chars.len() <= chars.len() {
        if chars[col..col + needle_chars.len()] == needle_chars[..] {
            spans.push((row, col, col + needle_chars.len() - 1));
            col += needle_chars.len();
        } else {
            col += 1;
        }
    }
    spans
}

// Reversed style over the selected cells of one pane. Spans carry
// (row, first col, last col inclusive) in text-grid coordinates; the
// pane rect maps them to screen cells. `style` decides the paint:
// REVERSED for selections, yellow for search hits.
fn paint_spans(frame: &mut Frame, pane: &PaneState, spans: &SpanList, style: Style) {
    let buf = frame.buffer_mut();
    for (row, c0, c1) in spans {
        let y = pane.rect.y + *row as u16;
        if y >= pane.rect.y + pane.rect.h {
            continue;
        }
        for col in *c0..=*c1 {
            let x = pane.rect.x + col as u16;
            if x >= pane.rect.x + pane.rect.w {
                break;
            }
            buf[(x, y)].set_style(style);
        }
    }
}

// Key hints shown alongside "copy mode": the client reserves the last
// terminal row for this bar (see the client's initial Resize), so it
// always has somewhere to draw and never gets overwritten by pane
// content.
const COPY_KEYS: &str = "hjkl move | { } prompt | ctrl+o yank cmd | v select | q exit";

// One status row on the last screen line, over everything else. Always
// on, with the active mode leftmost so it is never the part that gets
// cut off; in copy mode it also carries the scroll position and the
// key hints above (including ctrl+o, which has no other affordance).
fn draw_hint(frame: &mut Frame, hint: Hint, theme: &Theme) {
    let area = frame.area();
    let row = area.height.saturating_sub(1);
    let text = match hint {
        Hint::None => " input mode  ctrl+a leader ".to_string(),
        Hint::Copy(None) => format!(" copy mode  {COPY_KEYS} "),
        Hint::Copy(Some((offset, total))) => {
            format!(" copy mode {offset}/{total}  {COPY_KEYS} ")
        }
        Hint::Select => " copy mode select  y yank | esc cancel ".to_string(),
        Hint::Search(needle) => format!(" search: {needle} "),
    };
    let style = Style::new()
        .fg(theme.palette.hint_fg)
        .bg(theme.palette.hint_bg);
    let line = Line::from(vec![Span::styled(text, style)]);
    let para = Paragraph::new(line).style(style);
    frame.render_widget(
        para,
        RRect {
            x: 0,
            y: row,
            width: area.width,
            height: 1,
        },
    );
}

// tree.rects leaves a 1-cell gutter between siblings that no pane rect
// covers. Paint it as a vertical or horizontal line character spanning
// the overlap of the two adjacent panes; it lights when either side is
// focused. Overlap, not exact alignment, so nested layouts work.
fn paint_gutters(frame: &mut Frame, panes: &[PaneState], focused: PaneId, theme: &Theme) {
    for (i, a) in panes.iter().enumerate() {
        for b in &panes[i + 1..] {
            let (ra, rb) = (a.rect, b.rect);
            // Vertical gutter: b starts where a's right gutter column is.
            let vgap = if ra.x + ra.w < rb.x {
                Some((ra.x + ra.w, a, b))
            } else if rb.x + rb.w < ra.x {
                Some((rb.x + rb.w, b, a))
            } else {
                None
            };
            if let Some((gx, left, right)) = vgap {
                let (l, r) = (left.rect, right.rect);
                let y0 = l.y.max(r.y);
                let y1 = (l.y + l.h).min(r.y + r.h);
                if y1 > y0 && r.x - (l.x + l.w) == 1 {
                    let hot = left.id == focused || right.id == focused;
                    let style = Style::new().fg(if hot {
                        theme.palette.focused_gutter
                    } else {
                        theme.palette.gutter
                    });
                    for y in y0..y1 {
                        frame.buffer_mut()[(gx, y)].set_symbol("│").set_style(style);
                    }
                }
                continue;
            }
            // Horizontal gutter: b starts where a's bottom gutter row is.
            let hgap = if ra.y + ra.h < rb.y {
                Some((ra.y + ra.h, a, b))
            } else if rb.y + rb.h < ra.y {
                Some((rb.y + rb.h, b, a))
            } else {
                None
            };
            if let Some((gy, top, bottom)) = hgap {
                let (t, bo) = (top.rect, bottom.rect);
                let x0 = t.x.max(bo.x);
                let x1 = (t.x + t.w).min(bo.x + bo.w);
                if x1 > x0 && bo.y - (t.y + t.h) == 1 {
                    let hot = top.id == focused || bottom.id == focused;
                    let style = Style::new().fg(if hot {
                        theme.palette.focused_gutter
                    } else {
                        theme.palette.gutter
                    });
                    for x in x0..x1 {
                        frame.buffer_mut()[(x, gy)].set_symbol("─").set_style(style);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Palette;
    use corral_core::tree;
    use ratatui::{Terminal as TuiTerminal, backend::TestBackend};

    fn pane(id: u32, x: u16, y: u16, w: u16, h: u16, text: &str) -> PaneState {
        PaneState {
            id,
            rect: tree::Rect { x, y, w, h },
            text: text.into(),
            cursor: None,
            app_cursor: false,
            scroll: None,
            total_scrollback: 0,
            lines: vec![],
        }
    }

    fn panes() -> Vec<PaneState> {
        vec![
            pane(1, 0, 0, 50, 10, "pane-one\nsecond line"),
            pane(2, 51, 0, 50, 10, "pane-two"),
        ]
    }

    // A sentinel per surface, distinct from the bundled theme's real
    // colors: render tests assert against these fields, not the palette a
    // theme file happens to carry, so a Catppuccin tweak cannot silently
    // break a rendering test.
    fn test_theme() -> Theme {
        Theme {
            name: "test".into(),
            palette: Palette {
                gutter: Color::Rgb(1, 1, 1),
                focused_gutter: Color::Rgb(2, 2, 2),
                cursor_bg: Color::Rgb(3, 3, 3),
                cursor_fg: Color::Rgb(4, 4, 4),
                search_bg: Color::Rgb(5, 5, 5),
                search_fg: Color::Rgb(6, 6, 6),
                current_hit_bg: Color::Rgb(7, 7, 7),
                current_hit_fg: Color::Rgb(8, 8, 8),
                hint_bg: Color::Rgb(9, 9, 9),
                hint_fg: Color::Rgb(10, 10, 10),
            },
        }
    }

    fn draw_at(
        width: u16,
        height: u16,
        panes: &[PaneState],
        focused: u32,
    ) -> ratatui::buffer::Buffer {
        draw_full(width, height, panes, focused, Hint::None, &[], &[], None)
    }

    fn draw_with_spans(
        width: u16,
        height: u16,
        panes: &[PaneState],
        focused: u32,
        hint: Hint,
        spans: &[(u32, SpanList)],
    ) -> ratatui::buffer::Buffer {
        draw_full(width, height, panes, focused, hint, spans, &[], None)
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_full(
        width: u16,
        height: u16,
        panes: &[PaneState],
        focused: u32,
        hint: Hint,
        spans: &[(u32, SpanList)],
        search: &[(u32, SpanList)],
        cursor: Option<(usize, usize)>,
    ) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut term = TuiTerminal::new(backend).unwrap();
        let theme = test_theme();
        term.draw(|f| draw(f, panes, focused, hint, spans, search, &[], cursor, &theme))
            .unwrap();
        term.backend().buffer().clone()
    }

    fn row(buf: &ratatui::buffer::Buffer, y: u16, w: u16) -> String {
        (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect()
    }

    #[test]
    fn draws_two_panes_and_marks_focused_gutter() {
        let panes = panes();
        let buf = draw_at(101, 10, &panes, 2);
        let top = row(&buf, 0, 101);
        assert!(top.contains("pane-one"));
        assert!(top.contains("pane-two"));
        assert_eq!(buf[(50, 0)].symbol(), "│");
    }

    #[test]
    fn gutter_lights_when_either_neighbor_is_focused() {
        // One gutter column sits between the two panes; it highlights for
        // whichever side holds focus.
        let panes = panes();
        for focused in [1, 2] {
            let buf = draw_at(101, 10, &panes, focused);
            assert_eq!(buf[(50, 0)].fg, test_theme().palette.focused_gutter);
        }
    }

    #[test]
    fn unfocused_gutter_stays_dim() {
        // Nested layout: pane 1 left, panes 2 and 3 stacked on the right.
        // When pane 3 has focus, the x=50 gutter is not adjacent to it and
        // must stay dim while the horizontal gutter lights.
        let panes = vec![
            pane(1, 0, 0, 50, 10, "one"),
            pane(2, 51, 0, 50, 4, "two"),
            pane(3, 51, 5, 50, 5, "three"),
        ];
        let buf = draw_at(101, 10, &panes, 3);
        assert_eq!(buf[(50, 0)].fg, test_theme().palette.gutter);
        assert_eq!(buf[(60, 4)].fg, test_theme().palette.focused_gutter);
        assert_eq!(buf[(60, 4)].symbol(), "─");
    }

    #[test]
    fn gutter_column_is_the_only_highlighted_cell() {
        let panes = panes();
        let buf = draw_at(101, 10, &panes, 2);
        for x in 0..101u16 {
            if x == 50 {
                continue;
            }
            let got = buf[(x, 5)].fg;
            assert_ne!(
                got,
                test_theme().palette.focused_gutter,
                "cell ({x},5) fg {got:?}"
            );
        }
    }

    #[test]
    fn multiline_text_lands_on_consecutive_rows() {
        let panes = panes();
        let buf = draw_at(101, 10, &panes, 1);
        assert!(row(&buf, 0, 101).contains("pane-one"));
        assert!(row(&buf, 1, 101).contains("second line"));
    }

    #[test]
    fn pane_text_is_clipped_to_its_rect() {
        // A client that is not the sizing client is handed rects narrower
        // than the width the panes reflow at, so a pane's text can be wider
        // than its rect. The surplus has to be cut at the rect edge rather
        // than painted over the neighbour.
        let panes = vec![
            pane(1, 0, 0, 10, 3, "AAAAAAAAAAAAAAAAAAAAAAAA"),
            pane(2, 11, 0, 9, 3, "BBBBBBBBB\nBBBBBBBBB\nBBBBBBBBB"),
        ];
        // Two rows taller than the panes: the status row always owns the
        // last screen line.
        let buf = draw_at(20, 5, &panes, 1);
        assert_eq!(row(&buf, 0, 20), "AAAAAAAAAA│BBBBBBBBB");
        assert_eq!(row(&buf, 1, 20), "          │BBBBBBBBB");
        assert_eq!(row(&buf, 2, 20), "          │BBBBBBBBB");
        assert_eq!(row(&buf, 3, 20), "                    ");
    }

    #[test]
    fn text_fills_the_rect_up_to_the_gutter_edge() {
        // No padding: a full-width line runs to the rect's last column;
        // the gutter itself (col 50 here) carries the line character.
        let panes = vec![
            pane(1, 0, 0, 50, 10, &"x".repeat(50)),
            pane(2, 51, 0, 50, 10, "b"),
        ];
        let buf = draw_at(101, 10, &panes, 1);
        assert_eq!(buf[(49, 0)].symbol(), "x");
        assert_eq!(buf[(50, 0)].symbol(), "│");
    }

    #[test]
    fn text_clips_at_the_rect_boundary() {
        let panes = vec![pane(7, 0, 0, 10, 3, "a-very-long-line-that-overflows")];
        let buf = draw_at(20, 5, &panes, 7);
        // w 10 with no padding fits 10 columns of text.
        assert!(row(&buf, 0, 20).contains("a-very-lon"));
        assert!(!row(&buf, 0, 20).contains("a-very-long"));
    }

    #[test]
    fn empty_pane_text_draws_nothing_but_the_rect() {
        let panes = vec![pane(3, 0, 0, 8, 4, "")];
        let buf = draw_at(10, 4, &panes, 3);
        assert_eq!(row(&buf, 0, 10), " ".repeat(10));
    }

    #[test]
    fn focused_gutter_fills_the_full_height() {
        let panes = panes();
        let buf = draw_at(101, 10, &panes, 2);
        // Row 9 is the always-on status line, which paints over the
        // gutter on the last row; the client reserves that row so no
        // pane or gutter is ever expected to draw there.
        for y in 0..9u16 {
            assert_eq!(buf[(50, y)].symbol(), "│");
            assert_eq!(buf[(50, y)].fg, test_theme().palette.focused_gutter);
        }
    }

    #[test]
    fn copy_mode_hint_renders_on_the_last_row() {
        let panes = panes();
        let backend = TestBackend::new(101, 10);
        let mut term = TuiTerminal::new(backend).unwrap();
        let theme = test_theme();
        term.draw(|f| {
            draw(
                f,
                &panes,
                1,
                Hint::Copy(Some((12, 96))),
                &[],
                &[],
                &[],
                None,
                &theme,
            )
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        assert!(row(&buf, 9, 101).contains("copy mode"));
        assert!(row(&buf, 9, 101).contains("12/96"));
        // Content rows stay untouched.
        assert!(row(&buf, 0, 101).contains("pane-one"));
    }

    #[test]
    fn copy_mode_hint_without_position_shows_mode_only() {
        let panes = panes();
        let backend = TestBackend::new(101, 10);
        let mut term = TuiTerminal::new(backend).unwrap();
        let theme = test_theme();
        term.draw(|f| draw(f, &panes, 1, Hint::Copy(None), &[], &[], &[], None, &theme))
            .unwrap();
        let buf = term.backend().buffer().clone();
        assert!(row(&buf, 9, 101).contains("copy mode"));
        assert!(!row(&buf, 9, 101).contains("/"));
    }

    #[test]
    fn selection_spans_render_reversed() {
        let panes = panes();
        let buf = draw_with_spans(101, 10, &panes, 1, Hint::Select, &[(1, vec![(0, 0, 3)])]);
        // The first four cells of pane one carry the reversed modifier.
        for x in 0..4u16 {
            assert!(
                buf[(x, 0)]
                    .modifier
                    .contains(ratatui::style::Modifier::REVERSED),
                "cell ({x},0) not reversed"
            );
        }
        // Cell 4 of row 0 is outside the span.
        assert!(
            !buf[(4, 0)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        // The select hint renders too.
        assert!(row(&buf, 9, 101).contains("select"));
    }

    #[test]
    fn selection_spans_clip_at_the_pane_rect() {
        // Span reaches past the pane width; nothing paints into the
        // gutter or the neighbor.
        let panes = panes();
        let buf = draw_with_spans(101, 10, &panes, 1, Hint::None, &[(1, vec![(0, 0, 500)])]);
        for x in 50..101u16 {
            assert!(
                !buf[(x, 0)]
                    .modifier
                    .contains(ratatui::style::Modifier::REVERSED)
            );
        }
    }

    #[test]
    fn selection_spans_for_an_unknown_pane_are_ignored() {
        let panes = panes();
        let buf = draw_with_spans(101, 10, &panes, 1, Hint::None, &[(99, vec![(0, 0, 5)])]);
        assert!(row(&buf, 0, 101).contains("pane-two"));
        assert!(
            !buf[(51, 0)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
    }

    #[test]
    fn styled_runs_paint_fg_bg_and_attributes() {
        use corral_core::emulation::{CellAttrs, CellColor, StyledLine, StyledRun};
        let mut p = pane(1, 0, 0, 50, 10, "red bold plain");
        p.lines = vec![StyledLine {
            runs: vec![
                StyledRun {
                    text: "red".into(),
                    fg: CellColor::Indexed(1),
                    bg: CellColor::Default,
                    attrs: CellAttrs {
                        bold: true,
                        ..CellAttrs::default()
                    },
                },
                StyledRun {
                    text: " bold".into(),
                    fg: CellColor::Indexed(1),
                    bg: CellColor::Default,
                    attrs: CellAttrs {
                        bold: true,
                        ..CellAttrs::default()
                    },
                },
                StyledRun {
                    text: " plain".into(),
                    fg: CellColor::Default,
                    bg: CellColor::Rgb(10, 20, 30),
                    attrs: CellAttrs::default(),
                },
            ],
        }];
        let panes = vec![p];
        let buf = draw_at(60, 10, &panes, 1);
        assert_eq!(buf[(0, 0)].fg, Color::Indexed(1), "red run fg");
        assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD), "bold run");
        assert_eq!(buf[(9, 0)].bg, Color::Rgb(10, 20, 30), "rgb bg run paints");
        assert_eq!(buf[(9, 0)].fg, Color::Reset, "plain run keeps default fg");
    }

    #[test]
    fn styled_lines_fall_back_to_plain_text_when_empty() {
        let panes = panes();
        let buf = draw_at(101, 10, &panes, 1);
        assert!(row(&buf, 0, 101).contains("pane-one"));
    }

    #[test]
    fn copy_mode_cursor_paints_one_cell() {
        let panes = panes();
        let buf = draw_full(101, 10, &panes, 1, Hint::Copy(None), &[], &[], Some((2, 4)));
        assert_eq!(
            buf[(4, 2)].bg,
            test_theme().palette.cursor_bg,
            "cursor cell carries its bg"
        );
        assert_eq!(buf[(5, 2)].bg, Color::Reset, "neighbor cells untouched");
    }

    #[test]
    fn copy_mode_cursor_clips_at_the_pane_rect() {
        let panes = panes();
        let buf = draw_full(
            101,
            10,
            &panes,
            1,
            Hint::Copy(None),
            &[],
            &[],
            Some((2, 500)),
        );
        assert_eq!(
            buf[(50, 2)].bg,
            Color::Reset,
            "cursor never leaves the pane"
        );
    }

    #[test]
    fn copy_mode_cursor_only_on_the_focused_pane() {
        let panes = panes();
        let buf = draw_full(101, 10, &panes, 2, Hint::Copy(None), &[], &[], Some((0, 0)));
        assert_eq!(
            buf[(0, 0)].bg,
            Color::Reset,
            "unfocused pane shows no cursor"
        );
    }

    #[test]
    fn search_spans_find_every_occurrence_per_row() {
        let spans = search_spans("abc abc\nxabcx\nnope", "abc");
        assert_eq!(spans, vec![(0, 0, 2), (0, 4, 6), (1, 1, 3)]);
    }

    #[test]
    fn search_spans_of_an_empty_needle_is_empty() {
        assert!(search_spans("anything", "").is_empty());
    }

    #[test]
    fn search_spans_highlight_on_screen() {
        let panes = panes();
        let hit_spans = search_spans(&panes[0].text, "pane");
        let buf = draw_with_spans(
            101,
            10,
            &panes,
            1,
            Hint::Copy(None),
            &[(1, hit_spans.clone())],
        );
        // The search spans carry the same highlight style as selections
        // here (draw_with_spans paints REVERSED); the client passes the
        // search style through draw_full.
        assert!(
            buf[(0, 0)].modifier.contains(Modifier::REVERSED),
            "match start highlighted"
        );
        assert_eq!(hit_spans.len(), 1, "only pane-one's first row matches");
    }

    #[test]
    fn search_hits_render_in_the_theme_search_color() {
        // The dedicated search style paints the theme's colors, not
        // reversed: distinct from a selection span (S3-5, now theme-driven
        // per S4-5).
        let panes = panes();
        let hit_spans = search_spans(&panes[0].text, "pane");
        let buf = draw_full(101, 10, &panes, 1, Hint::None, &[], &[(1, hit_spans)], None);
        let theme = test_theme();
        assert_eq!(buf[(0, 0)].bg, theme.palette.search_bg);
        assert_eq!(buf[(0, 0)].fg, theme.palette.search_fg);
        assert!(!buf[(0, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn current_hit_spans_matches_one_row_only() {
        let spans = current_hit_spans("abc abc\nxabcx\nnope", "abc", 1);
        assert_eq!(spans, vec![(1, 1, 3)], "only row one's match");
        assert!(current_hit_spans("abc abc", "abc", 2).is_empty());
        assert!(current_hit_spans("abc abc", "", 0).is_empty());
    }

    #[test]
    fn current_hit_paints_the_theme_color_over_the_search_color() {
        let panes = panes();
        let hit_spans = search_spans(&panes[0].text, "pane");
        let current = current_hit_spans(&panes[0].text, "pane", 0);
        let backend = TestBackend::new(101, 10);
        let mut term = TuiTerminal::new(backend).unwrap();
        let theme = test_theme();
        term.draw(|f| {
            draw(
                f,
                &panes,
                1,
                Hint::None,
                &[],
                &[(1, hit_spans)],
                &[(1, current)],
                None,
                &theme,
            )
        })
        .unwrap();
        assert_eq!(
            term.backend().buffer()[(0, 0)].bg,
            theme.palette.current_hit_bg,
            "the hit the user is on paints over the plain search color"
        );
        assert_eq!(
            term.backend().buffer()[(0, 0)].fg,
            theme.palette.current_hit_fg
        );
    }
}
