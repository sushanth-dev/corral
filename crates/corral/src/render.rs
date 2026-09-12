use corral_core::tree::PaneId;
use corrald::protocol::PaneState;
use ratatui::Frame;
use ratatui::layout::Rect as RRect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph};

// The focused pane's surrounding gutter lights up so focus is visible.
const FOCUSED_GUTTER: Color = Color::Indexed(245);

pub fn draw(frame: &mut Frame, panes: &[PaneState], focused: PaneId) {
    for PaneState {
        id: _,
        rect,
        text,
        cursor: _,
    } in panes
    {
        let rr = RRect {
            x: rect.x,
            y: rect.y,
            width: rect.w,
            height: rect.h,
        };
        let lines: Vec<Line> = text.lines().map(Line::from).collect();
        // No padding: the tree's 1-cell gutter is the whole separator; the
        // old side padding doubled its visual width.
        let para = Paragraph::new(lines);
        frame.render_widget(para, rr);
    }
    paint_gutters(frame, panes, focused);
}

// tree.rects leaves a 1-cell gutter between siblings that no pane rect
// covers, so the block-over-rect alone never highlights it. Paint the
// gutter cell strip between the focused pane and each adjacent sibling.
fn paint_gutters(frame: &mut Frame, panes: &[PaneState], focused: PaneId) {
    let Some(fp) = panes.iter().find(|p| p.id == focused) else {
        return;
    };
    let f = fp.rect;
    let gutter = Style::new().bg(FOCUSED_GUTTER);
    for PaneState { id: _, rect, .. } in panes {
        let rect = *rect;
        if rect == f {
            continue;
        }
        // Sibling starts where the gutter column begins: focused pane on
        // the left. The gutter row belongs to the sibling span.
        let right_of_focused =
            f.y == rect.y && f.h == rect.h && f.x + f.w < rect.x && rect.x - (f.x + f.w) == 1;
        // Focused pane on the right of this sibling.
        let left_of_focused =
            f.y == rect.y && f.h == rect.h && rect.x + rect.w < f.x && f.x - (rect.x + rect.w) == 1;
        // Sibling below the gutter row: focused pane on top.
        let below_focused =
            f.x == rect.x && f.w == rect.w && f.y + f.h < rect.y && rect.y - (f.y + f.h) == 1;
        // Focused pane below the gutter row.
        let above_focused =
            f.x == rect.x && f.w == rect.w && rect.y + rect.h < f.y && f.y - (rect.y + rect.h) == 1;
        if right_of_focused || left_of_focused {
            let gx = if right_of_focused {
                f.x + f.w
            } else {
                rect.x + rect.w
            };
            frame.render_widget(
                Block::default().style(gutter),
                RRect {
                    x: gx,
                    y: f.y,
                    width: 1,
                    height: f.h,
                },
            );
        }
        if below_focused || above_focused {
            let gy = if below_focused {
                f.y + f.h
            } else {
                rect.y + rect.h
            };
            frame.render_widget(
                Block::default().style(gutter),
                RRect {
                    x: f.x,
                    y: gy,
                    width: f.w,
                    height: 1,
                },
            );
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
        let backend = TestBackend::new(width, height);
        let mut term = TuiTerminal::new(backend).unwrap();
        term.draw(|f| draw(f, panes, focused)).unwrap();
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
        assert_eq!(buf[(50, 0)].bg, ratatui::style::Color::Indexed(245));
        assert_eq!(buf[(0, 0)].bg, ratatui::style::Color::Reset);
    }

    #[test]
    fn gutter_lights_when_either_neighbor_is_focused() {
        // One gutter column sits between the two panes; it highlights for
        // whichever side holds focus.
        let panes = panes();
        for focused in [1, 2] {
            let buf = draw_at(101, 10, &panes, focused);
            assert_eq!(buf[(50, 0)].bg, ratatui::style::Color::Indexed(245));
        }
    }

    #[test]
    fn unfocused_gutter_stays_reset() {
        // Nested layout: pane 1 left, panes 2 and 3 stacked on the right.
        // When pane 3 has focus, the x=50 gutter is not adjacent to it and
        // must stay Reset while the horizontal gutter lights.
        let panes = vec![
            pane(1, 0, 0, 50, 10, "one"),
            pane(2, 51, 0, 50, 4, "two"),
            pane(3, 51, 5, 50, 5, "three"),
        ];
        let buf = draw_at(101, 10, &panes, 3);
        assert_eq!(buf[(50, 0)].bg, ratatui::style::Color::Reset);
        assert_eq!(buf[(60, 4)].bg, ratatui::style::Color::Indexed(245));
    }

    #[test]
    fn gutter_column_is_the_only_highlighted_cell() {
        let panes = panes();
        let buf = draw_at(101, 10, &panes, 2);
        for x in 0..101u16 {
            if x == 50 {
                continue;
            }
            let got = buf[(x, 5)].bg;
            assert_eq!(got, ratatui::style::Color::Reset, "cell ({x},5) bg {got:?}");
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
        // the gutter itself (col 50 here) stays its own cell.
        let panes = vec![pane(1, 0, 0, 50, 10, &"x".repeat(50))];
        let buf = draw_at(101, 10, &panes, 1);
        assert_eq!(buf[(49, 0)].symbol(), "x");
        assert_eq!(buf[(50, 0)].symbol(), " ");
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
            assert_eq!(buf[(50, y)].bg, ratatui::style::Color::Indexed(245));
        }
    }
}
