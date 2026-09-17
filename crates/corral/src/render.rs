use crate::theme::Theme;
use corral_core::emulation::CellColor;
use corral_core::tree::PaneId;
use corrald::protocol::PaneState;
use ratatui::Frame;
use ratatui::layout::Rect as RRect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

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
    picker: Option<&crate::input::Picker>,
    spans: &[(PaneId, SpanList)],
    search: &[(PaneId, SpanList)],
    current: &[(PaneId, SpanList)],
    cursor: Option<(usize, usize)>,
    workspace: &str,
    home: &str,
    clock: &str,
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
    let dir = pane_dir(panes.iter().find(|p| p.id == focused));
    draw_status(
        frame, workspace, dir, &hint, panes, focused, home, clock, theme,
    );
    // The picker paints last, over the panes and the status row both.
    if let Some(picker) = picker {
        draw_picker(frame, picker, theme);
    }
}

/// The directory the status bar shows: the focused pane's OSC 7 working
/// directory, empty when the pane has not reported one.
fn pane_dir(pane: Option<&PaneState>) -> &str {
    pane.map(|p| p.pwd.as_str()).unwrap_or("")
}

// What divides the bar's zones. The clock carries it too, so the bar
// reads as `name | directory | panes | clock`.
const SEPARATOR: &str = " | ";

/// The directory as the bar shows it: `$HOME` collapses to `~` and every
/// component but the last is cut to its first character, so a deep path
/// fits without losing the part that says where we are. Only absolute
/// paths are shortened; anything else passes through.
fn shorten_path(path: &str, home: &str) -> String {
    if !path.starts_with('/') {
        return path.to_string();
    }
    // Matched per component, so `/Users/sushantho` is not read as living
    // under `/Users/sushanth`.
    let under_home = if home.is_empty() {
        None
    } else {
        path.strip_prefix(home)
            .filter(|tail| tail.is_empty() || tail.starts_with('/'))
    };
    let parts: Vec<&str> = under_home
        .unwrap_or(path)
        .split('/')
        .filter(|p| !p.is_empty())
        .collect();
    let mut out = if under_home.is_some() {
        "~".to_string()
    } else {
        String::new()
    };
    if parts.is_empty() {
        // Home itself, or the root.
        if out.is_empty() {
            out.push('/');
        }
        return out;
    }
    let last = parts.len() - 1;
    for (i, part) in parts.iter().enumerate() {
        out.push('/');
        if i == last {
            out.push_str(part);
        } else {
            out.push_str(&first_char(part));
        }
    }
    out
}

/// The one character a shortened component keeps. A dotfile keeps its
/// dot, since a bare `.` would read as the `..` entry.
fn first_char(part: &str) -> String {
    let mut chars = part.chars();
    match chars.next() {
        None => String::new(),
        Some('.') => match chars.next() {
            Some(c) => format!(".{c}"),
            None => ".".to_string(),
        },
        Some(c) => c.to_string(),
    }
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

/// The status row on the last screen line, over everything else, always
/// present. Four zones: the workspace on the left, then the focused
/// pane's directory and its place in the frame, then the clock flush
/// right. Only the directory gives way when the row is narrow.
#[allow(clippy::too_many_arguments)]
fn draw_status(
    frame: &mut Frame,
    workspace: &str,
    dir: &str,
    hint: &Hint,
    panes: &[PaneState],
    focused: PaneId,
    home: &str,
    clock: &str,
    theme: &Theme,
) {
    let area = frame.area();
    let row = area.height.saturating_sub(1);
    let panes_zone = pane_zone(panes, focused);
    let spans = status_row(
        area.width as usize,
        workspace,
        dir,
        hint,
        &panes_zone,
        home,
        clock,
        theme,
    );
    let para = Paragraph::new(Line::from(spans)).style(
        Style::new()
            .fg(theme.palette.hint_fg)
            .bg(theme.palette.hint_bg),
    );
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

/// The mode's name as the statusline's leftmost block shows it, Neovim
/// style: a colored block that says the mode in words. Copy carries its
/// viewport position, since that is the mode's whole state.
fn mode_block(hint: &Hint) -> String {
    match hint {
        Hint::None => " NORMAL ".to_string(),
        Hint::Copy(None) => " COPY ".to_string(),
        Hint::Copy(Some((offset, total))) => format!(" COPY {offset}/{total} "),
        Hint::Select => " SELECT ".to_string(),
        Hint::Search(_) => " SEARCH ".to_string(),
    }
}

/// Compose one status row, lualine style: a mode block in its own
/// background at the left edge, then the name, the directory, the pane
/// index and the clock as segments on the bar's own background, the
/// clock flush right. The mode block's background is the theme's accent,
/// which is what makes the mode readable at a glance; the name keeps the
/// same accent as its foreground.
#[allow(clippy::too_many_arguments)]
fn status_row(
    width: usize,
    workspace: &str,
    dir: &str,
    hint: &Hint,
    panes_zone: &str,
    home: &str,
    clock: &str,
    theme: &Theme,
) -> Vec<Span<'static>> {
    // A needle is typed from its front, so it gives way at the right; a
    // path reads from its last component, so it gives way at the left.
    // The directory zone is the only one that shrinks: everything else
    // on the row is fixed-width.
    let block = mode_block(hint);
    let dir_full = match hint {
        Hint::Search(needle) => format!("search: {needle}"),
        _ => shorten_path(dir, home),
    };
    let mut rest = String::new();
    for zone in [dir_full.as_str(), panes_zone] {
        if zone.is_empty() {
            continue;
        }
        rest.push_str(SEPARATOR);
        rest.push_str(zone);
    }
    let fixed = block.chars().count()
        + workspace.chars().count()
        + SEPARATOR.chars().count()
        + clock.chars().count();
    let dir_room = width
        .saturating_sub(fixed)
        .saturating_sub(rest.chars().count() - dir_full.chars().count());
    let dir_zone = match hint {
        Hint::Search(_) => truncate_right(&dir_full, dir_room),
        _ => truncate_left(&dir_full, dir_room),
    };
    let mut rest = String::new();
    for zone in [dir_zone.as_str(), panes_zone] {
        if zone.is_empty() {
            continue;
        }
        rest.push_str(SEPARATOR);
        rest.push_str(zone);
    }
    let used = block.chars().count()
        + workspace.chars().count()
        + rest.chars().count()
        + SEPARATOR.chars().count()
        + clock.chars().count();
    vec![
        Span::styled(
            block,
            Style::new()
                .fg(theme.palette.cursor_fg)
                .bg(theme.palette.focused_gutter)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            workspace.to_string(),
            Style::new().fg(theme.palette.focused_gutter),
        ),
        Span::raw(rest),
        Span::raw(" ".repeat(width.saturating_sub(used))),
        Span::raw(format!("{SEPARATOR}{clock}")),
    ]
}

/// The focused pane's place in the frame, `3/3`, counted in the frame's
/// own order, which is layout order. Empty when the focused pane is not
/// in the frame.
fn pane_zone(panes: &[PaneState], focused: PaneId) -> String {
    panes
        .iter()
        .position(|p| p.id == focused)
        .map(|i| format!("{}/{}", i + 1, panes.len()))
        .unwrap_or_default()
}

/// The binding picker over the panes, fzf style: a centered floating
/// window with the query as a prompt line, the filtered bindings below,
/// and the selected line in the accent. Enter runs it, esc closes; the
/// footer says so. The list comes from the same table the leader
/// dispatches through, so the two can never disagree.
fn draw_picker(frame: &mut Frame, picker: &crate::input::Picker, theme: &Theme) {
    let area = frame.area();
    let matches = crate::input::picker_matches(&picker.query);
    let bindings = crate::input::leader_bindings();
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(" {} ", picker.query),
        Style::new()
            .fg(theme.palette.cursor_fg)
            .bg(theme.palette.cursor_bg),
    )));
    for (row, idx) in matches.iter().enumerate() {
        let (keys, what, _) = bindings[*idx];
        let text = format!("  {keys:<4}  {what}");
        let line = if row == picker.selected {
            Line::from(Span::styled(
                text,
                Style::new()
                    .fg(theme.palette.cursor_fg)
                    .bg(theme.palette.focused_gutter),
            ))
        } else {
            Line::from(Span::raw(text))
        };
        lines.push(line);
    }
    if matches.is_empty() {
        lines.push(Line::from(Span::raw("  no match")));
    }
    lines.push(Line::from(Span::raw(String::new())));
    lines.push(Line::from(Span::raw(
        "  type filters | enter runs | esc closes",
    )));
    // Sized to the content, capped to the screen minus the status row.
    let body = lines
        .iter()
        .map(|l| l.width())
        .max()
        .unwrap_or(0)
        .max(" corral keys ".len());
    let width = (body as u16 + 2).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(1));
    let style = Style::new()
        .fg(theme.palette.hint_fg)
        .bg(theme.palette.hint_bg);
    let block = Block::bordered()
        .title(Span::styled(
            " corral keys ",
            Style::new().fg(theme.palette.focused_gutter),
        ))
        .border_style(style);
    frame.render_widget(
        Paragraph::new(lines).style(style).block(block),
        RRect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        },
    );
}

/// Trim `text` to `room` characters, cutting from the left behind a
/// leading ellipsis, so the end of a long path survives. `room` of 0
/// leaves nothing.
fn truncate_left(text: &str, room: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= room {
        return text.to_string();
    }
    if room == 0 {
        return String::new();
    }
    let tail: String = chars[chars.len() - (room - 1)..].iter().collect();
    format!("…{tail}")
}

/// Trim `text` to `room` characters, cutting from the right behind a
/// trailing ellipsis, so the start of a long needle survives.
fn truncate_right(text: &str, room: usize) -> String {
    if text.chars().count() <= room {
        return text.to_string();
    }
    if room == 0 {
        return String::new();
    }
    let head: String = text.chars().take(room - 1).collect();
    format!("{head}…")
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
            pwd: String::new(),
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

    // The bar's outer zones are fixed for every test that is not about
    // them, so the assertions read the parts under test rather than the
    // clock's minute. Home is a parameter of `draw`, not read from the
    // environment, so these tests do not depend on whose machine runs
    // them.
    const WORKSPACE: &str = "corral-test";
    const HOME: &str = "/Users/example";
    const CLOCK: &str = "14:23 16-Sep-26";

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
        draw_keys(
            width, height, panes, focused, hint, spans, search, cursor, None,
        )
    }

    // `draw_full` with the `t` picker open.
    #[allow(clippy::too_many_arguments)]
    fn draw_keys(
        width: u16,
        height: u16,
        panes: &[PaneState],
        focused: u32,
        hint: Hint,
        spans: &[(u32, SpanList)],
        search: &[(u32, SpanList)],
        cursor: Option<(usize, usize)>,
        picker: Option<&crate::input::Picker>,
    ) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut term = TuiTerminal::new(backend).unwrap();
        let theme = test_theme();
        term.draw(|f| {
            draw(
                f,
                panes,
                focused,
                hint,
                picker,
                spans,
                search,
                &[],
                cursor,
                WORKSPACE,
                HOME,
                CLOCK,
                &theme,
            )
        })
        .unwrap();
        term.backend().buffer().clone()
    }
    fn row(buf: &ratatui::buffer::Buffer, y: u16, w: u16) -> String {
        (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect()
    }

    // The whole screen as text, for the assertions that care where a
    // string sits rather than which row it is on.
    fn screen_text(buf: &ratatui::buffer::Buffer, height: u16, width: u16) -> String {
        (0..height)
            .map(|y| row(buf, y, width))
            .collect::<Vec<_>>()
            .join("\n")
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
    fn the_status_bar_shows_the_workspace_the_focused_panes_directory_and_the_clock() {
        let mut panes = panes();
        panes[0].pwd = format!("{HOME}/Dev/app");
        let buf = draw_at(101, 10, &panes, 1);
        let bar = row(&buf, 9, 101);
        assert!(bar.starts_with(" NORMAL corral-test | "), "got {bar:?}");
        assert!(bar.contains("~/D/app"), "got {bar:?}");
        assert!(bar.ends_with(" | 14:23 16-Sep-26"), "got {bar:?}");
    }

    #[test]
    fn the_bar_shows_the_focused_panes_directory_and_no_other() {
        let mut panes = panes();
        panes[0].pwd = "/one".into();
        panes[1].pwd = "/two".into();
        let buf = draw_at(101, 10, &panes, 2);
        let bar = row(&buf, 9, 101);
        assert!(bar.contains("/two"), "focused pane's dir, got {bar:?}");
        assert!(!bar.contains("/one"), "unfocused pane's dir, got {bar:?}");
        // The pane's top-left corner carries its own text, not a label.
        assert_eq!(row(&buf, 0, 5), "pane-");
    }

    #[test]
    fn a_directory_too_long_for_the_bar_keeps_its_tail() {
        let mut panes = panes();
        panes[0].pwd = format!("/long/{}", "d".repeat(120));
        let buf = draw_at(60, 10, &panes, 1);
        let bar = row(&buf, 9, 60);
        assert!(bar.contains('…'), "the cut must be marked, got {bar:?}");
        assert!(
            bar.contains(&"d".repeat(10)),
            "the tail must survive the cut, got {bar:?}"
        );
    }

    #[test]
    fn the_bar_shows_its_outer_zones_when_the_pane_reports_no_directory() {
        let panes = vec![pane(1, 0, 0, 10, 3, "pane-content")];
        // Wide enough for every zone: at 40 the mode block would crowd the
        // clock off the right edge.
        let buf = draw_at(50, 4, &panes, 1);
        let bar = row(&buf, 3, 50);
        assert_eq!(bar, " NORMAL corral-test | 1/1        | 14:23 16-Sep-26");
        assert!(!bar.contains("pane-content"), "the bar owns the last row");
    }

    #[test]
    fn the_bar_paints_the_session_name_in_the_accent_color() {
        let panes = panes();
        let buf = draw_at(101, 10, &panes, 1);
        let theme = test_theme();
        let offset = " NORMAL ".chars().count() as u16;
        for x in offset..offset + WORKSPACE.chars().count() as u16 {
            assert_eq!(
                buf[(x, 9)].fg,
                theme.palette.focused_gutter,
                "name cell {x}"
            );
        }
        // The zones after the name keep the bar's own foreground.
        let after = offset + WORKSPACE.chars().count() as u16 + 1;
        assert_eq!(buf[(after, 9)].fg, theme.palette.hint_fg, "cell {after}");
        assert_eq!(buf[(after, 9)].bg, theme.palette.hint_bg, "cell {after}");
    }

    #[test]
    fn the_bar_reports_the_focused_panes_index_in_layout_order() {
        let panes = vec![
            pane(1, 0, 0, 50, 10, "one"),
            pane(2, 51, 0, 50, 4, "two"),
            pane(3, 51, 5, 50, 5, "three"),
        ];
        let buf = draw_at(101, 10, &panes, 3);
        assert!(
            row(&buf, 9, 101).contains("3/3"),
            "got {:?}",
            row(&buf, 9, 101)
        );
        let buf = draw_at(101, 10, &panes, 1);
        assert!(
            row(&buf, 9, 101).contains("1/3"),
            "got {:?}",
            row(&buf, 9, 101)
        );
    }

    #[test]
    fn the_bar_reports_no_index_for_a_pane_that_is_not_in_the_frame() {
        let panes = panes();
        let buf = draw_at(101, 10, &panes, 99);
        let bar = row(&buf, 9, 101);
        assert!(!bar.contains('/'), "got {bar:?}");
    }

    #[test]
    fn a_path_under_home_collapses_to_a_tilde_and_first_letters() {
        assert_eq!(shorten_path("/Users/example/Dev/app", HOME), "~/D/app");
        assert_eq!(
            shorten_path("/Users/example/.config/corral", HOME),
            "~/.c/corral"
        );
        assert_eq!(shorten_path("/Users/example", HOME), "~");
        assert_eq!(shorten_path("/Users/example/", HOME), "~");
    }

    #[test]
    fn a_path_outside_home_keeps_its_own_first_letters() {
        assert_eq!(shorten_path("/tmp/project", HOME), "/t/project");
        assert_eq!(shorten_path("/", HOME), "/");
        // A sibling that merely starts with the same characters is not
        // under home.
        assert_eq!(shorten_path("/Users/example2/x", HOME), "/U/e/x");
    }

    #[test]
    fn the_last_component_is_never_abbreviated() {
        assert_eq!(
            shorten_path("/Users/example/agent-mux", HOME),
            "~/agent-mux"
        );
        assert_eq!(shorten_path("/a/b", HOME), "/a/b");
    }

    #[test]
    fn a_dotfile_component_keeps_its_dot() {
        assert_eq!(first_char(".config"), ".c");
        assert_eq!(first_char(".."), "..");
        assert_eq!(first_char("."), ".");
        assert_eq!(first_char(""), "");
        assert_eq!(first_char("src"), "s");
    }

    #[test]
    fn a_relative_path_passes_through_untouched() {
        assert_eq!(shorten_path("src/main.rs", HOME), "src/main.rs");
        assert_eq!(shorten_path("", HOME), "");
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
    fn the_picker_lists_the_leader_bindings_over_the_panes() {
        let panes = panes();
        let picker = crate::input::Picker::new();
        let buf = draw_keys(
            101,
            20,
            &panes,
            1,
            Hint::Copy(Some((12, 96))),
            &[],
            &[],
            None,
            Some(&picker),
        );
        let screen = screen_text(&buf, 20, 101);
        assert!(screen.contains(" COPY 12/96"), "got {screen:?}");
        assert!(screen.contains("h/l"), "got {screen:?}");
        assert!(screen.contains("type filters"), "got {screen:?}");
        // The box sits over the panes: this cell is inside pane one and
        // inside the box, on an unselected row, and it carries the box's
        // background. (Row 5 is the selected binding, in the accent.)
        assert_eq!(buf[(40, 6)].bg, test_theme().palette.hint_bg);
    }

    #[test]
    fn the_picker_prompt_shows_the_query() {
        let panes = panes();
        let mut picker = crate::input::Picker::new();
        picker.query = "spl".into();
        let buf = draw_keys(
            101,
            20,
            &panes,
            1,
            Hint::None,
            &[],
            &[],
            None,
            Some(&picker),
        );
        let screen = screen_text(&buf, 20, 101);
        assert!(screen.contains(" spl "), "got {screen:?}");
        // Splitting is the only binding whose description or keys carry
        // "spl"; the rest are filtered out of the list.
        assert!(!screen.contains("yank"), "got {screen:?}");
    }

    #[test]
    fn the_picker_marks_the_selected_line_with_the_accent() {
        let panes = panes();
        let picker = crate::input::Picker::new();
        let buf = draw_keys(
            101,
            20,
            &panes,
            1,
            Hint::None,
            &[],
            &[],
            None,
            Some(&picker),
        );
        // The prompt line ` {query} ` carries the cursor background. Scan
        // every column on every row for it; the box is centered so no
        // single column is guaranteed to be inside it.
        let theme = test_theme();
        let prompt = (0..20u16)
            .find_map(|y| {
                (0..101u16)
                    .find(|&x| buf[(x, y)].bg == theme.palette.cursor_bg)
                    .map(|_| y)
            })
            .expect("prompt row with the query background");
        // One row below the prompt, the first match sits in the accent.
        assert!(
            (0..101u16).any(|x| buf[(x, prompt + 1)].bg == theme.palette.focused_gutter),
            "the selected row is in the accent"
        );
    }

    #[test]
    fn the_picker_shortcut_leaves_the_bar_alone_and_opens_the_picker() {
        let mut panes = panes();
        panes[0].pwd = "/tmp/project".into();
        // Closed: the bar reports the mode and the directory, and there
        // is no box.
        let buf = draw_keys(101, 20, &panes, 1, Hint::Copy(None), &[], &[], None, None);
        let closed = row(&buf, 19, 101);
        assert!(
            !screen_text(&buf, 20, 101).contains("corral keys"),
            "no picker while it is closed"
        );
        // Open: the same bar, with the box over the panes.
        let picker = crate::input::Picker::new();
        let buf = draw_keys(
            101,
            20,
            &panes,
            1,
            Hint::Copy(None),
            &[],
            &[],
            None,
            Some(&picker),
        );
        assert_eq!(row(&buf, 19, 101), closed, "the bar does not change");
        assert!(
            screen_text(&buf, 20, 101).contains("corral keys"),
            "the picker opens over the panes"
        );
        // The status row is reserved either way, never reclaimed:
        // reclaiming it would resize every pane and reflow their PTYs.
        assert_eq!(buf.area.height, 20);
    }

    #[test]
    fn a_typed_search_needle_stays_on_the_bar_while_the_picker_is_open() {
        let panes = panes();
        let picker = crate::input::Picker::new();
        let buf = draw_keys(
            101,
            10,
            &panes,
            1,
            Hint::Search("needle".into()),
            &[],
            &[],
            None,
            Some(&picker),
        );
        assert!(
            row(&buf, 9, 101).contains("search: needle"),
            "got {:?}",
            row(&buf, 9, 101)
        );
    }

    #[test]
    fn the_mode_block_names_the_mode_in_hand() {
        assert_eq!(mode_block(&Hint::None), " NORMAL ");
        assert_eq!(mode_block(&Hint::Copy(None)), " COPY ");
        assert_eq!(mode_block(&Hint::Copy(Some((12, 96)))), " COPY 12/96 ");
        assert_eq!(mode_block(&Hint::Select), " SELECT ");
        assert_eq!(mode_block(&Hint::Search("nee".into())), " SEARCH ");
    }

    #[test]
    fn the_statusline_leads_with_the_mode_block() {
        let panes = panes();
        for (hint, block) in [
            (Hint::None, " NORMAL "),
            (Hint::Copy(Some((3, 9))), " COPY 3/9 "),
            (Hint::Select, " SELECT "),
        ] {
            let buf = draw_keys(101, 10, &panes, 1, hint.clone(), &[], &[], None, None);
            assert!(
                row(&buf, 9, 101).starts_with(block),
                "got {:?}",
                row(&buf, 9, 101)
            );
        }
    }

    #[test]
    fn the_mode_block_paints_in_the_accent() {
        let panes = panes();
        let buf = draw_at(101, 10, &panes, 1);
        let theme = test_theme();
        assert_eq!(buf[(0, 9)].bg, theme.palette.focused_gutter);
        assert_eq!(buf[(0, 9)].fg, theme.palette.cursor_fg);
        // The block is bold; the rest of the bar is not.
        assert!(
            buf[(0, 9)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        let after = " NORMAL ".chars().count() as u16;
        assert!(
            !buf[(after, 9)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
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
        assert!(row(&buf, 9, 101).contains("SELECT"));
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
                None,
                &[],
                &[(1, hit_spans)],
                &[(1, current)],
                None,
                WORKSPACE,
                HOME,
                CLOCK,
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
