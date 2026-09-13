use corral_core::tree::PaneId;
use corrald::protocol::PaneState;
use ratatui::Frame;
use ratatui::layout::Rect as RRect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

// Gutters between panes render as thin line characters. Adjacent to the
// focused pane they light up so focus is visible.
const GUTTER: Color = Color::Indexed(238);
const FOCUSED_GUTTER: Color = Color::Indexed(245);

/// Selection highlight spans for one pane: (row, first col, last col
/// inclusive) in text-grid coordinates.
pub type SpanList = Vec<(usize, usize, usize)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hint {
    None,
    /// Copy mode active; carries the focused pane's viewport position
    /// (`None` while pinned to the bottom).
    Copy(Option<(usize, usize)>),
    /// Copy mode with an active selection.
    Select,
}

pub fn draw(
    frame: &mut Frame,
    panes: &[PaneState],
    focused: PaneId,
    hint: Hint,
    spans: &[(PaneId, SpanList)],
) {
    for PaneState {
        id: _, rect, text, ..
    } in panes
    {
        let rr = RRect {
            x: rect.x,
            y: rect.y,
            width: rect.w,
            height: rect.h,
        };
        let lines: Vec<Line> = text.lines().map(Line::from).collect();
        // No padding: the tree's 1-cell gutter is the whole separator.
        let para = Paragraph::new(lines);
        frame.render_widget(para, rr);
    }
    for (pane_id, sel_spans) in spans {
        let Some(p) = panes.iter().find(|p| p.id == *pane_id) else {
            continue;
        };
        paint_spans(frame, p, sel_spans);
    }
    paint_gutters(frame, panes, focused);
    if hint != Hint::None {
        draw_hint(frame, hint);
    }
}

// Reversed style over the selected cells of one pane. Spans carry
// (row, first col, last col inclusive) in text-grid coordinates; the
// pane rect maps them to screen cells.
fn paint_spans(frame: &mut Frame, pane: &PaneState, spans: &SpanList) {
    let style = Style::new().add_modifier(ratatui::style::Modifier::REVERSED);
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

// One status row on the last screen line, over everything else. Shows
// the active mode and, in copy mode, the scroll position when the
// viewport is off the bottom.
fn draw_hint(frame: &mut Frame, hint: Hint) {
    let area = frame.area();
    let row = area.height.saturating_sub(1);
    let text = match hint {
        Hint::None => return,
        Hint::Copy(None) => " copy mode ".to_string(),
        Hint::Copy(Some((offset, total))) => {
            format!(" copy mode {offset}/{total} ")
        }
        Hint::Select => " copy mode select ".to_string(),
    };
    let style = Style::new().fg(Color::Black).bg(Color::Indexed(245));
    let width = text.len() as u16;
    let line = Line::from(vec![Span::styled(text, style)]);
    let para = Paragraph::new(line);
    frame.render_widget(
        para,
        RRect {
            x: 0,
            y: row,
            width,
            height: 1,
        },
    );
}

// tree.rects leaves a 1-cell gutter between siblings that no pane rect
// covers. Paint it as a vertical or horizontal line character spanning
// the overlap of the two adjacent panes; it lights when either side is
// focused. Overlap, not exact alignment, so nested layouts work.
fn paint_gutters(frame: &mut Frame, panes: &[PaneState], focused: PaneId) {
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
                    let style = Style::new().fg(if hot { FOCUSED_GUTTER } else { GUTTER });
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
                    let style = Style::new().fg(if hot { FOCUSED_GUTTER } else { GUTTER });
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
        }
    }

    fn panes() -> Vec<PaneState> {
        vec![
            pane(1, 0, 0, 50, 10, "pane-one\nsecond line"),
            pane(2, 51, 0, 50, 10, "pane-two"),
        ]
    }

    fn draw_at(
        width: u16,
        height: u16,
        panes: &[PaneState],
        focused: u32,
    ) -> ratatui::buffer::Buffer {
        draw_with_spans(width, height, panes, focused, Hint::None, &[])
    }

    fn draw_with_spans(
        width: u16,
        height: u16,
        panes: &[PaneState],
        focused: u32,
        hint: Hint,
        spans: &[(u32, SpanList)],
    ) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut term = TuiTerminal::new(backend).unwrap();
        term.draw(|f| draw(f, panes, focused, hint, spans)).unwrap();
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
            assert_eq!(buf[(50, 0)].fg, ratatui::style::Color::Indexed(245));
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
        assert_eq!(buf[(50, 0)].fg, ratatui::style::Color::Indexed(238));
        assert_eq!(buf[(60, 4)].fg, ratatui::style::Color::Indexed(245));
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
                ratatui::style::Color::Indexed(245),
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
        for y in 0..10u16 {
            assert_eq!(buf[(50, y)].symbol(), "│");
            assert_eq!(buf[(50, y)].fg, ratatui::style::Color::Indexed(245));
        }
    }

    #[test]
    fn copy_mode_hint_renders_on_the_last_row() {
        let panes = panes();
        let backend = TestBackend::new(101, 10);
        let mut term = TuiTerminal::new(backend).unwrap();
        term.draw(|f| draw(f, &panes, 1, Hint::Copy(Some((12, 96))), &[]))
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
        term.draw(|f| draw(f, &panes, 1, Hint::Copy(None), &[]))
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
}
