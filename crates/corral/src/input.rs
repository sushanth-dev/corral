use corral_core::tree::Dir;
use corrald::protocol::ScrollTarget;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::selection::SelectMode;

/// Client input mode. Copy mode (Ctrl+a [) routes keys to scrollback
/// navigation and swallows everything else; nothing binds a bare key in
/// input mode. Select is copy mode with an active v/V selection. Search
/// is the `/` prompt: typed characters build the needle, Enter submits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Input,
    Copy,
    Select(SelectMode),
    Search,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Focus(Dir),
    // s splits vertically, v horizontally; the daemon splits the focused
    // pane along the requested direction.
    Split(Dir),
    Quit,
    Send(Vec<u8>),
    EnterCopy,
    CopyScroll(ScrollTarget),
    ExitCopy,
    BeginSelect(SelectMode),
    /// Motion while a selection is active: move the selection cursor by
    /// (drow, dcol) within the visible grid. Coordinates are
    /// viewport-relative; the recorded simplification means no scrolling
    /// mid-selection.
    SelectMove {
        drow: isize,
        dcol: isize,
    },
    /// Copy-mode cursor motion (h/j/k/l and arrows). The client moves
    /// its viewport cursor and scrolls the pane when the cursor pushes
    /// past the top or bottom edge.
    CopyCursorMove {
        drow: isize,
        dcol: isize,
    },
    Yank,
    CancelSelect,
    /// `/` in copy mode: open the search prompt.
    BeginSearch,
    /// `?` in copy mode: search prompt, first match found upward.
    BeginSearchReverse,
    /// A printable character typed into the search prompt.
    SearchChar(char),
    /// Backspace in the search prompt.
    SearchBackspace,
    /// Enter in the search prompt: submit the needle.
    SearchSubmit,
    /// Esc in the search prompt: drop the needle, back to copy mode.
    SearchCancel,
    /// `n` in copy mode: repeat the last search forward.
    SearchNext,
    /// `N` in copy mode: repeat the last search backward.
    SearchPrev,
    /// Leader `c`: erase the focused pane's scrollback.
    ClearHistory,
    /// `{` in copy mode: jump to the previous prompt row.
    PromptPrev,
    /// `}` in copy mode: jump to the next prompt row.
    PromptNext,
    /// Leader `o`: move focus to the next pane, wrapping.
    FocusNext,
    /// Leader `t`: open the keymap dialogue.
    ToggleHint,
}

/// One entry in the leader table: the key after Ctrl+a, its description
/// for the keymap dialogue, and the action it dispatches. A single table
/// backs both, so the dialogue can never list a binding `leader_action`
/// does not also produce. A description is `<keys> <what>`: the dialogue
/// reads the first word as the key to press and the rest as its effect.
struct LeaderEntry {
    code: KeyCode,
    description: &'static str,
    action: fn() -> Action,
}

const LEADER_TABLE: &[LeaderEntry] = &[
    LeaderEntry {
        code: KeyCode::Char('h'),
        description: "h/l focus",
        action: || Action::Focus(Dir::Horizontal),
    },
    LeaderEntry {
        code: KeyCode::Char('l'),
        description: "h/l focus",
        action: || Action::Focus(Dir::Horizontal),
    },
    LeaderEntry {
        code: KeyCode::Char('j'),
        description: "j/k focus",
        action: || Action::Focus(Dir::Vertical),
    },
    LeaderEntry {
        code: KeyCode::Char('k'),
        description: "j/k focus",
        action: || Action::Focus(Dir::Vertical),
    },
    // tmux geometry: % splits right (side by side, cut in width =
    // Dir::Horizontal here), " splits below (stacked = Dir::Vertical).
    LeaderEntry {
        code: KeyCode::Char('%'),
        description: "% split right",
        action: || Action::Split(Dir::Horizontal),
    },
    LeaderEntry {
        code: KeyCode::Char('"'),
        description: "\" split down",
        action: || Action::Split(Dir::Vertical),
    },
    LeaderEntry {
        code: KeyCode::Char('o'),
        description: "o next pane",
        action: || Action::FocusNext,
    },
    LeaderEntry {
        code: KeyCode::Left,
        description: "h/l focus",
        action: || Action::Focus(Dir::Horizontal),
    },
    LeaderEntry {
        code: KeyCode::Right,
        description: "h/l focus",
        action: || Action::Focus(Dir::Horizontal),
    },
    LeaderEntry {
        code: KeyCode::Up,
        description: "j/k focus",
        action: || Action::Focus(Dir::Vertical),
    },
    LeaderEntry {
        code: KeyCode::Down,
        description: "j/k focus",
        action: || Action::Focus(Dir::Vertical),
    },
    LeaderEntry {
        code: KeyCode::Char('['),
        description: "[ copy mode",
        action: || Action::EnterCopy,
    },
    LeaderEntry {
        code: KeyCode::Char('c'),
        description: "c clear history",
        action: || Action::ClearHistory,
    },
    LeaderEntry {
        code: KeyCode::Char('d'),
        description: "d quit",
        action: || Action::Quit,
    },
    LeaderEntry {
        code: KeyCode::Char('t'),
        description: "t keymaps",
        action: || Action::ToggleHint,
    },
];

/// Every leader binding as `(keys, what)`, in table order, with
/// duplicates (h/l and the arrows both focus) folded into one entry.
pub fn leader_keys() -> Vec<(&'static str, &'static str)> {
    let mut keys: Vec<(&'static str, &'static str)> = Vec::new();
    for entry in LEADER_TABLE {
        let pair = entry
            .description
            .split_once(' ')
            .unwrap_or((entry.description, ""));
        if !keys.contains(&pair) {
            keys.push(pair);
        }
    }
    keys
}

// The Ctrl+a leader is the only key corral consumes in input mode. In
// copy mode the leader still arms (tmux-style) so Ctrl+a c reaches its
// action from scrollback; every other copy key is
// consumed: vi keys move the cursor, everything else does nothing, and
// no byte ever reaches the pane. `half_page` is the pane height for
// Ctrl+d/Ctrl+u; it only matters in copy mode.

/// The shared leader table: the key after Ctrl+a. Used by input mode
/// and, since the leader arms there too, by copy mode.
fn leader_action(code: KeyCode, _mods: KeyModifiers) -> Option<Action> {
    LEADER_TABLE
        .iter()
        .find(|entry| entry.code == code)
        .map(|entry| (entry.action)())
}

pub fn handle(
    ev: KeyEvent,
    armed: &mut bool,
    mode: &Mode,
    app_cursor: bool,
    half_page: u16,
) -> Option<Action> {
    if *mode == Mode::Search {
        return match ev.code {
            KeyCode::Char(c) => Some(Action::SearchChar(c)),
            KeyCode::Backspace => Some(Action::SearchBackspace),
            KeyCode::Enter => Some(Action::SearchSubmit),
            KeyCode::Esc => Some(Action::SearchCancel),
            _ => None,
        };
    }
    if *mode == Mode::Copy {
        // The leader works in copy mode (tmux-style): Ctrl+a arms, the
        // next key goes through the shared leader table (Ctrl+a c clear
        // history lands from here).
        if matches!(
            (ev.code, ev.modifiers),
            (KeyCode::Char('a'), KeyModifiers::CONTROL)
        ) {
            *armed = true;
            return None;
        }
        if *armed {
            *armed = false;
            return leader_action(ev.code, ev.modifiers);
        }
        return match (ev.code, ev.modifiers) {
            // libghostty's ScrollViewport::Delta is "up is negative": k
            // (earlier lines) is negative, j (later lines) is positive.
            // h/j/k/l and arrows move the copy cursor instead of raw
            // scrolling; the cursor drags the viewport at the edges.
            (KeyCode::Char('h'), _) | (KeyCode::Left, _) => {
                Some(Action::CopyCursorMove { drow: 0, dcol: -1 })
            }
            (KeyCode::Char('l'), _) | (KeyCode::Right, _) => {
                Some(Action::CopyCursorMove { drow: 0, dcol: 1 })
            }
            (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
                Some(Action::CopyCursorMove { drow: 1, dcol: 0 })
            }
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
                Some(Action::CopyCursorMove { drow: -1, dcol: 0 })
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                Some(Action::CopyScroll(ScrollTarget::Delta(half_page as isize)))
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => Some(Action::CopyScroll(
                ScrollTarget::Delta(-(half_page as isize)),
            )),
            (KeyCode::Char('g'), _) => Some(Action::CopyScroll(ScrollTarget::Top)),
            (KeyCode::Char('G'), _) => Some(Action::CopyScroll(ScrollTarget::Bottom)),
            (KeyCode::Char('/'), _) => Some(Action::BeginSearch),
            (KeyCode::Char('?'), _) => Some(Action::BeginSearchReverse),
            (KeyCode::Char('n'), _) => Some(Action::SearchNext),
            (KeyCode::Char('N'), _) => Some(Action::SearchPrev),
            (KeyCode::Char('{'), _) => Some(Action::PromptPrev),
            (KeyCode::Char('}'), _) => Some(Action::PromptNext),
            (KeyCode::Char('v'), _) => Some(Action::BeginSelect(SelectMode::Span)),
            (KeyCode::Char('V'), _) => Some(Action::BeginSelect(SelectMode::Rect)),
            (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => Some(Action::ExitCopy),
            // Typing, focus keys: swallowed, never forwarded.
            _ => None,
        };
    }
    if matches!(mode, Mode::Select(_)) {
        return match (ev.code, ev.modifiers) {
            (KeyCode::Char('v'), _) => Some(Action::CancelSelect),
            (KeyCode::Char('V'), _) => Some(Action::CancelSelect),
            (KeyCode::Char('y'), _) | (KeyCode::Enter, _) => Some(Action::Yank),
            (KeyCode::Esc, _) => Some(Action::CancelSelect),
            (KeyCode::Char('q'), _) => Some(Action::ExitCopy),
            (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
                Some(Action::SelectMove { drow: 1, dcol: 0 })
            }
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
                Some(Action::SelectMove { drow: -1, dcol: 0 })
            }
            (KeyCode::Char('h'), _) | (KeyCode::Left, _) => {
                Some(Action::SelectMove { drow: 0, dcol: -1 })
            }
            (KeyCode::Char('l'), _) | (KeyCode::Right, _) => {
                Some(Action::SelectMove { drow: 0, dcol: 1 })
            }
            _ => None,
        };
    }
    if *armed {
        *armed = false;
        // Ctrl+a Ctrl+a passes a real 0x01 through, tmux-style; it only
        // exists in input mode, where Ctrl+a means "send".
        if matches!(
            (ev.code, ev.modifiers),
            (KeyCode::Char('a'), KeyModifiers::CONTROL)
        ) && *mode == Mode::Input
        {
            return Some(Action::Send(vec![0x01]));
        }
        return leader_action(ev.code, ev.modifiers);
    }
    match (ev.code, ev.modifiers) {
        (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
            *armed = true;
            None
        }
        // Ctrl+letter (and Ctrl+@, Ctrl+[, ... ) forwards as the byte the
        // char maps to under & 0x1f, so shell bindings (Ctrl+u, Ctrl+e,
        // Ctrl+w) and program bindings (Ctrl+c, Ctrl+d) behave exactly as
        // in a plain terminal. The leader itself is reserved above.
        (KeyCode::Char(c), KeyModifiers::CONTROL)
            if c.is_ascii_lowercase() || ('@'..='_').contains(&c) || c == ' ' =>
        {
            Some(Action::Send(vec![c as u8 & 0x1f]))
        }
        (KeyCode::Char(c), m) if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT => {
            Some(Action::Send(c.to_string().into_bytes()))
        }
        (KeyCode::Char(c), m)
            if m == KeyModifiers::ALT || m == KeyModifiers::ALT | KeyModifiers::SHIFT =>
        {
            let mut bytes = vec![0x1b];
            bytes.extend(c.to_string().as_bytes());
            Some(Action::Send(bytes))
        }
        (KeyCode::Char(c), m)
            if m == KeyModifiers::ALT | KeyModifiers::CONTROL && c.is_ascii_lowercase() =>
        {
            Some(Action::Send(vec![0x1b, c as u8 - b'a' + 1]))
        }
        (KeyCode::Enter, _) => Some(Action::Send(b"\r".to_vec())),
        (KeyCode::Backspace, _) => Some(Action::Send(b"\x7f".to_vec())),
        // crossterm reports Shift+Tab as BackTab.
        (KeyCode::BackTab, _) | (KeyCode::Tab, KeyModifiers::SHIFT) => {
            Some(Action::Send(b"\x1b[Z".to_vec()))
        }
        (KeyCode::Tab, _) => Some(Action::Send(b"\t".to_vec())),
        (KeyCode::Esc, _) => Some(Action::Send(b"\x1b".to_vec())),
        (KeyCode::Up, m) => Some(Action::Send(arrow('A', m, app_cursor))),
        (KeyCode::Down, m) => Some(Action::Send(arrow('B', m, app_cursor))),
        (KeyCode::Right, m) => Some(Action::Send(arrow('C', m, app_cursor))),
        (KeyCode::Left, m) => Some(Action::Send(arrow('D', m, app_cursor))),
        (KeyCode::Home, m) => Some(Action::Send(home_end('H', m, app_cursor))),
        (KeyCode::End, m) => Some(Action::Send(home_end('F', m, app_cursor))),
        (KeyCode::PageUp, m) => Some(Action::Send(tilde(5, m))),
        (KeyCode::PageDown, m) => Some(Action::Send(tilde(6, m))),
        (KeyCode::Insert, m) => Some(Action::Send(tilde(2, m))),
        (KeyCode::Delete, m) => Some(Action::Send(tilde(3, m))),
        (KeyCode::F(n), m) => fkey(n, m).map(Action::Send),
        _ => None,
    }
}

/// One thing a mouse event asks for. Distinct from `Action` because a
/// wheel tick in input mode does two things at once (enter copy mode,
/// then scroll it), which a single `Action` cannot carry.
#[derive(Debug, PartialEq)]
pub enum MouseAction {
    FocusPane(corral_core::tree::PaneId),
    SetSplitRatio {
        at: corral_core::tree::PaneId,
        ratio: f32,
    },
    Scroll {
        pane: corral_core::tree::PaneId,
        delta: isize,
    },
    EnterCopyThenScroll {
        pane: corral_core::tree::PaneId,
        delta: isize,
    },
}

const WHEEL_DELTA: isize = 3;

/// The pane whose rect contains (col, row), if any.
fn pane_at(
    panes: &[corrald::protocol::PaneState],
    col: u16,
    row: u16,
) -> Option<corral_core::tree::PaneId> {
    panes
        .iter()
        .find(|p| {
            col >= p.rect.x
                && col < p.rect.x + p.rect.w
                && row >= p.rect.y
                && row < p.rect.y + p.rect.h
        })
        .map(|p| p.id)
}

/// Hit-tests a gutter cell: not inside any pane, but bordered by panes on
/// both sides along one axis. Returns the bordering pane nearest the
/// origin (the "at" pane the daemon resizes relative to, via
/// `Node::set_ratio_near`), the split's axis, and the combined rect the
/// two sides span (used to turn a later drag position into a ratio).
fn gutter_at(
    panes: &[corrald::protocol::PaneState],
    col: u16,
    row: u16,
) -> Option<(corral_core::tree::PaneId, Dir, corral_core::tree::Rect)> {
    let shares_row =
        |p: &&corrald::protocol::PaneState| row >= p.rect.y && row < p.rect.y + p.rect.h;
    let left = panes
        .iter()
        .filter(shares_row)
        .find(|p| p.rect.x + p.rect.w == col);
    let right = panes
        .iter()
        .filter(shares_row)
        .find(|p| p.rect.x == col + 1);
    if let (Some(l), Some(r)) = (left, right) {
        let x0 = panes
            .iter()
            .filter(|p| p.rect.x + p.rect.w == col)
            .map(|p| p.rect.x)
            .min()
            .unwrap_or(l.rect.x);
        let x1 = panes
            .iter()
            .filter(|p| p.rect.x == col + 1)
            .map(|p| p.rect.x + p.rect.w)
            .max()
            .unwrap_or(r.rect.x + r.rect.w);
        let y0 = l.rect.y.min(r.rect.y);
        let y1 = (l.rect.y + l.rect.h).max(r.rect.y + r.rect.h);
        return Some((
            l.id,
            Dir::Horizontal,
            corral_core::tree::Rect {
                x: x0,
                y: y0,
                w: x1 - x0,
                h: y1 - y0,
            },
        ));
    }
    let shares_col =
        |p: &&corrald::protocol::PaneState| col >= p.rect.x && col < p.rect.x + p.rect.w;
    let top = panes
        .iter()
        .filter(shares_col)
        .find(|p| p.rect.y + p.rect.h == row);
    let bottom = panes
        .iter()
        .filter(shares_col)
        .find(|p| p.rect.y == row + 1);
    if let (Some(t), Some(b)) = (top, bottom) {
        let y0 = panes
            .iter()
            .filter(|p| p.rect.y + p.rect.h == row)
            .map(|p| p.rect.y)
            .min()
            .unwrap_or(t.rect.y);
        let y1 = panes
            .iter()
            .filter(|p| p.rect.y == row + 1)
            .map(|p| p.rect.y + p.rect.h)
            .max()
            .unwrap_or(b.rect.y + b.rect.h);
        let x0 = t.rect.x.min(b.rect.x);
        let x1 = (t.rect.x + t.rect.w).max(b.rect.x + b.rect.w);
        return Some((
            t.id,
            Dir::Vertical,
            corral_core::tree::Rect {
                x: x0,
                y: y0,
                w: x1 - x0,
                h: y1 - y0,
            },
        ));
    }
    None
}

/// Dispatches a mouse event: a click focuses the pane under it or starts
/// tracking a gutter drag, a drag on a tracked gutter yields a ratio
/// change, and wheel ticks scroll the pane under the pointer, opening
/// copy mode first when they move toward history in the focused pane. A
/// wheel tick outside every pane (the status bar row, a gutter) does
/// nothing. `drag` persists the gutter a button-down started tracking
/// across the following drag events.
pub fn handle_mouse(
    ev: crossterm::event::MouseEvent,
    panes: &[corrald::protocol::PaneState],
    mode: &Mode,
    focused: corral_core::tree::PaneId,
    drag: &mut Option<(corral_core::tree::PaneId, Dir, corral_core::tree::Rect)>,
) -> Option<MouseAction> {
    use crossterm::event::{MouseButton, MouseEventKind};
    match ev.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(id) = pane_at(panes, ev.column, ev.row) {
                *drag = None;
                return Some(MouseAction::FocusPane(id));
            }
            *drag = gutter_at(panes, ev.column, ev.row);
            None
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let (at, dir, group) = (*drag)?;
            let ratio = match dir {
                Dir::Horizontal => {
                    (ev.column.saturating_sub(group.x)) as f32 / group.w.max(1) as f32
                }
                Dir::Vertical => (ev.row.saturating_sub(group.y)) as f32 / group.h.max(1) as f32,
            };
            Some(MouseAction::SetSplitRatio {
                at,
                ratio: ratio.clamp(0.0, 1.0),
            })
        }
        MouseEventKind::Up(MouseButton::Left) => {
            *drag = None;
            None
        }
        MouseEventKind::ScrollUp => Some(scroll_action(
            mode,
            focused,
            pane_at(panes, ev.column, ev.row)?,
            -WHEEL_DELTA,
        )),
        MouseEventKind::ScrollDown => Some(scroll_action(
            mode,
            focused,
            pane_at(panes, ev.column, ev.row)?,
            WHEEL_DELTA,
        )),
        _ => None,
    }
}

/// A wheel tick on `pane`. Scrolling toward history opens copy mode when
/// the client is not already there, so what the wheel revealed can be
/// yanked; scrolling back toward the live screen is a plain viewport move
/// and never opens copy mode. The pane is named on the action because the
/// pointer picks it, not the daemon's focus. Copy mode only opens for the
/// focused pane, since the copy cursor and yank read that pane's state,
/// so a wheel tick on any other pane stays a plain viewport move.
fn scroll_action(
    mode: &Mode,
    focused: corral_core::tree::PaneId,
    pane: corral_core::tree::PaneId,
    delta: isize,
) -> MouseAction {
    if *mode == Mode::Input && pane == focused && delta < 0 {
        MouseAction::EnterCopyThenScroll { pane, delta }
    } else {
        MouseAction::Scroll { pane, delta }
    }
}

// xterm modifier parameter: shift adds 1, alt 2, ctrl 4 (1 = none).
fn mod_code(m: KeyModifiers) -> u8 {
    1 + u8::from(m.contains(KeyModifiers::SHIFT))
        + u8::from(m.contains(KeyModifiers::ALT)) * 2
        + u8::from(m.contains(KeyModifiers::CONTROL)) * 4
}

fn arrow(letter: char, m: KeyModifiers, app_cursor: bool) -> Vec<u8> {
    let code = mod_code(m);
    if code > 1 {
        format!("\x1b[1;{code}{letter}").into_bytes()
    } else if app_cursor {
        // The pane set DECCKM: arrows arrive as SS3, or vim ignores them.
        format!("\x1bO{letter}").into_bytes()
    } else {
        format!("\x1b[{letter}").into_bytes()
    }
}

fn home_end(letter: char, m: KeyModifiers, app_cursor: bool) -> Vec<u8> {
    arrow(letter, m, app_cursor)
}

fn tilde(n: u8, m: KeyModifiers) -> Vec<u8> {
    let code = mod_code(m);
    if code > 1 {
        format!("\x1b[{n};{code}~").into_bytes()
    } else {
        format!("\x1b[{n}~").into_bytes()
    }
}

fn fkey(n: u8, m: KeyModifiers) -> Option<Vec<u8>> {
    let code = mod_code(m);
    let seq = match n {
        1..=4 => {
            let l = (b'P' + n - 1) as char;
            if code > 1 {
                format!("\x1b[1;{code}{l}")
            } else {
                format!("\x1bO{l}")
            }
        }
        5..=12 => {
            let num = [15, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize];
            if code > 1 {
                format!("\x1b[{num};{code}~")
            } else {
                format!("\x1b[{num}~")
            }
        }
        _ => return None,
    };
    Some(seq.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventState, MouseButton, MouseEventKind};

    fn key(code: KeyCode, mods: KeyModifiers) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent {
            code,
            modifiers: mods,
            kind: crossterm::event::KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn sent(code: KeyCode, mods: KeyModifiers) -> Vec<u8> {
        sent_ac(code, mods, false)
    }

    fn sent_ac(code: KeyCode, mods: KeyModifiers, app_cursor: bool) -> Vec<u8> {
        let mut armed = false;
        match handle(key(code, mods), &mut armed, &Mode::Input, app_cursor, 0) {
            Some(Action::Send(b)) => b,
            other => panic!("{code:?} {mods:?} produced {other:?}, expected Send"),
        }
    }

    /// Arms the leader then presses `code` in input mode.
    fn leader_then(code: KeyCode, mods: KeyModifiers) -> Option<Action> {
        let mut armed = false;
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            &Mode::Input,
            false,
            0,
        );
        handle(key(code, mods), &mut armed, &Mode::Input, false, 0)
    }

    #[test]
    fn leader_then_h_focuses_left() {
        let mut armed = false;
        assert!(
            handle(
                key(KeyCode::Char('a'), KeyModifiers::CONTROL),
                &mut armed,
                &Mode::Input,
                false,
                0
            )
            .is_none()
        );
        assert!(armed);
        assert!(matches!(
            handle(
                key(KeyCode::Char('h'), KeyModifiers::NONE),
                &mut armed,
                &Mode::Input,
                false,
                0
            ),
            Some(Action::Focus(Dir::Horizontal))
        ));
        assert!(!armed);
    }

    #[test]
    fn leader_then_j_focuses_down_and_percent_quote_split() {
        let mut armed = false;
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            &Mode::Input,
            false,
            0,
        );
        assert!(matches!(
            handle(
                key(KeyCode::Char('j'), KeyModifiers::NONE),
                &mut armed,
                &Mode::Input,
                false,
                0
            ),
            Some(Action::Focus(Dir::Vertical))
        ));
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            &Mode::Input,
            false,
            0,
        );
        // tmux: % splits right (Dir::Horizontal cuts width), " splits
        // below (Dir::Vertical cuts height).
        assert!(matches!(
            handle(
                key(KeyCode::Char('%'), KeyModifiers::SHIFT),
                &mut armed,
                &Mode::Input,
                false,
                0
            ),
            Some(Action::Split(Dir::Horizontal))
        ));
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            &Mode::Input,
            false,
            0,
        );
        assert!(matches!(
            handle(
                key(KeyCode::Char('"'), KeyModifiers::SHIFT),
                &mut armed,
                &Mode::Input,
                false,
                0
            ),
            Some(Action::Split(Dir::Vertical))
        ));
    }

    #[test]
    fn leader_then_o_cycles_focus_and_arrows_focus() {
        assert!(matches!(
            leader_then(KeyCode::Char('o'), KeyModifiers::NONE),
            Some(Action::FocusNext)
        ));
        assert!(matches!(
            leader_then(KeyCode::Left, KeyModifiers::NONE),
            Some(Action::Focus(Dir::Horizontal))
        ));
        assert!(matches!(
            leader_then(KeyCode::Up, KeyModifiers::NONE),
            Some(Action::Focus(Dir::Vertical))
        ));
    }

    #[test]
    fn leader_then_d_quits() {
        let mut armed = false;
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            &Mode::Input,
            false,
            0,
        );
        assert!(matches!(
            handle(
                key(KeyCode::Char('d'), KeyModifiers::NONE),
                &mut armed,
                &Mode::Input,
                false,
                0
            ),
            Some(Action::Quit)
        ));
        assert!(!armed);
    }

    #[test]
    fn plain_typing_sends_bytes_and_is_not_eaten() {
        let mut armed = false;
        assert!(matches!(
            handle(key(KeyCode::Char('x'), KeyModifiers::NONE), &mut armed, &Mode::Input, false, 0),
            Some(Action::Send(b)) if b == b"x"
        ));
        assert!(!armed);
    }

    #[test]
    fn leader_sequence_unknown_key_disarms_and_sends_nothing() {
        let mut armed = false;
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            &Mode::Input,
            false,
            0,
        );
        assert!(
            handle(
                key(KeyCode::Char('z'), KeyModifiers::NONE),
                &mut armed,
                &Mode::Input,
                false,
                0
            )
            .is_none()
        );
        assert!(!armed);
    }

    #[test]
    fn enter_backspace_and_ctrl_chars_route_to_the_pane() {
        assert_eq!(sent(KeyCode::Enter, KeyModifiers::NONE), b"\r".to_vec());
        assert_eq!(
            sent(KeyCode::Backspace, KeyModifiers::NONE),
            b"\x7f".to_vec()
        );
        assert_eq!(
            sent(KeyCode::Char('c'), KeyModifiers::CONTROL),
            b"\x03".to_vec()
        );
        assert_eq!(
            sent(KeyCode::Char('d'), KeyModifiers::CONTROL),
            b"\x04".to_vec()
        );
    }

    #[test]
    fn ctrl_u_and_ctrl_e_forward_as_control_bytes() {
        assert_eq!(
            sent(KeyCode::Char('u'), KeyModifiers::CONTROL),
            b"\x15".to_vec()
        );
        assert_eq!(
            sent(KeyCode::Char('e'), KeyModifiers::CONTROL),
            b"\x05".to_vec()
        );
        // Every lowercase Ctrl+letter maps to its 0x01..=0x1a byte.
        for c in b'b'..=b'z' {
            let byte = c - b'a' + 1;
            assert_eq!(
                sent(KeyCode::Char(c as char), KeyModifiers::CONTROL),
                vec![byte]
            );
        }
    }

    #[test]
    fn second_ctrl_a_sends_a_literal_ctrl_a() {
        let mut armed = false;
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            &Mode::Input,
            false,
            0,
        );
        assert!(armed);
        assert!(matches!(
            handle(key(KeyCode::Char('a'), KeyModifiers::CONTROL), &mut armed, &Mode::Input, false, 0),
            Some(Action::Send(b)) if b == vec![0x01]
        ));
        assert!(!armed);
        // A fresh Ctrl+a re-arms afterwards.
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            &Mode::Input,
            false,
            0,
        );
        assert!(armed);
        assert!(matches!(
            handle(
                key(KeyCode::Char('d'), KeyModifiers::NONE),
                &mut armed,
                &Mode::Input,
                false,
                0
            ),
            Some(Action::Quit)
        ));
    }

    #[test]
    fn arrows_send_csi_sequences_by_default() {
        assert_eq!(sent(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A".to_vec());
        assert_eq!(sent(KeyCode::Down, KeyModifiers::NONE), b"\x1b[B".to_vec());
        assert_eq!(sent(KeyCode::Right, KeyModifiers::NONE), b"\x1b[C".to_vec());
        assert_eq!(sent(KeyCode::Left, KeyModifiers::NONE), b"\x1b[D".to_vec());
    }

    #[test]
    fn arrows_switch_to_ss3_when_the_pane_sets_decckm() {
        assert_eq!(
            sent_ac(KeyCode::Up, KeyModifiers::NONE, true),
            b"\x1bOA".to_vec()
        );
        assert_eq!(
            sent_ac(KeyCode::Down, KeyModifiers::NONE, true),
            b"\x1bOB".to_vec()
        );
        assert_eq!(
            sent_ac(KeyCode::Right, KeyModifiers::NONE, true),
            b"\x1bOC".to_vec()
        );
        assert_eq!(
            sent_ac(KeyCode::Left, KeyModifiers::NONE, true),
            b"\x1bOD".to_vec()
        );
        // CSI form still used with modifiers even in application mode.
        assert_eq!(
            sent_ac(KeyCode::Up, KeyModifiers::CONTROL, true),
            b"\x1b[1;5A".to_vec()
        );
    }

    #[test]
    fn modified_arrows_use_xterm_modifier_parameters() {
        assert_eq!(
            sent(KeyCode::Up, KeyModifiers::SHIFT),
            b"\x1b[1;2A".to_vec()
        );
        assert_eq!(
            sent(KeyCode::Left, KeyModifiers::ALT),
            b"\x1b[1;3D".to_vec()
        );
        assert_eq!(
            sent(KeyCode::Right, KeyModifiers::CONTROL),
            b"\x1b[1;5C".to_vec()
        );
        assert_eq!(
            sent(KeyCode::Down, KeyModifiers::SHIFT | KeyModifiers::CONTROL),
            b"\x1b[1;6B".to_vec()
        );
    }

    #[test]
    fn navigation_and_function_keys_forward_their_sequences() {
        assert_eq!(sent(KeyCode::Home, KeyModifiers::NONE), b"\x1b[H".to_vec());
        assert_eq!(sent(KeyCode::End, KeyModifiers::NONE), b"\x1b[F".to_vec());
        assert_eq!(
            sent(KeyCode::PageUp, KeyModifiers::NONE),
            b"\x1b[5~".to_vec()
        );
        assert_eq!(
            sent(KeyCode::PageDown, KeyModifiers::NONE),
            b"\x1b[6~".to_vec()
        );
        assert_eq!(
            sent(KeyCode::Delete, KeyModifiers::NONE),
            b"\x1b[3~".to_vec()
        );
        assert_eq!(
            sent(KeyCode::Insert, KeyModifiers::NONE),
            b"\x1b[2~".to_vec()
        );
        assert_eq!(
            sent(KeyCode::BackTab, KeyModifiers::NONE),
            b"\x1b[Z".to_vec()
        );
        assert_eq!(sent(KeyCode::Esc, KeyModifiers::NONE), b"\x1b".to_vec());
        assert_eq!(sent(KeyCode::Tab, KeyModifiers::NONE), b"\t".to_vec());
        assert_eq!(sent(KeyCode::F(1), KeyModifiers::NONE), b"\x1bOP".to_vec());
        assert_eq!(
            sent(KeyCode::F(5), KeyModifiers::NONE),
            b"\x1b[15~".to_vec()
        );
        assert_eq!(
            sent(KeyCode::F(12), KeyModifiers::NONE),
            b"\x1b[24~".to_vec()
        );
        assert_eq!(
            sent(KeyCode::Delete, KeyModifiers::CONTROL),
            b"\x1b[3;5~".to_vec()
        );
    }

    #[test]
    fn alt_and_ctrl_alt_chords_forward_escape_prefixed_bytes() {
        assert_eq!(
            sent(KeyCode::Char('b'), KeyModifiers::ALT),
            b"\x1bb".to_vec()
        );
        assert_eq!(
            sent(
                KeyCode::Char('b'),
                KeyModifiers::ALT | KeyModifiers::CONTROL
            ),
            b"\x1b\x02".to_vec()
        );
    }

    #[test]
    fn ctrl_special_chars_map_through_the_0x1f_mask() {
        assert_eq!(sent(KeyCode::Char(' '), KeyModifiers::CONTROL), vec![0x00]);
        assert_eq!(sent(KeyCode::Char('@'), KeyModifiers::CONTROL), vec![0x00]);
        assert_eq!(sent(KeyCode::Char('_'), KeyModifiers::CONTROL), vec![0x1f]);
    }

    #[test]
    fn unhandled_keys_return_none_without_arming() {
        let mut armed = false;
        assert!(
            handle(
                key(KeyCode::CapsLock, KeyModifiers::NONE),
                &mut armed,
                &Mode::Input,
                false,
                0
            )
            .is_none()
        );
        assert!(
            handle(
                key(KeyCode::F(13), KeyModifiers::NONE),
                &mut armed,
                &Mode::Input,
                false,
                0
            )
            .is_none()
        );
        assert!(!armed);
    }

    #[test]
    fn every_armed_leader_key_maps_to_the_planned_action() {
        // Exhaustive map check straight from the plan's keymap table.
        type Check = fn(&Action) -> bool;
        let cases: Vec<(KeyCode, Check)> = vec![
            (KeyCode::Char('h'), |a: &Action| {
                matches!(a, Action::Focus(Dir::Horizontal))
            }),
            (KeyCode::Char('l'), |a: &Action| {
                matches!(a, Action::Focus(Dir::Horizontal))
            }),
            (KeyCode::Char('j'), |a: &Action| {
                matches!(a, Action::Focus(Dir::Vertical))
            }),
            (KeyCode::Char('k'), |a: &Action| {
                matches!(a, Action::Focus(Dir::Vertical))
            }),
            (KeyCode::Char('o'), |a: &Action| {
                matches!(a, Action::FocusNext)
            }),
            (KeyCode::Char('c'), |a: &Action| {
                matches!(a, Action::ClearHistory)
            }),
            (KeyCode::Char('d'), |a: &Action| matches!(a, Action::Quit)),
            (KeyCode::Char('t'), |a: &Action| {
                matches!(a, Action::ToggleHint)
            }),
        ];
        for (code, check) in cases {
            let mut armed = false;
            handle(
                key(KeyCode::Char('a'), KeyModifiers::CONTROL),
                &mut armed,
                &Mode::Input,
                false,
                0,
            );
            let action = handle(
                key(code, KeyModifiers::NONE),
                &mut armed,
                &Mode::Input,
                false,
                0,
            )
            .unwrap_or_else(|| panic!("{code:?} produced no action"));
            assert!(check(&action), "{code:?} produced {action:?}");
            assert!(!armed, "{code:?} left the leader armed");
        }
    }

    #[test]
    fn leader_keys_lists_every_binding_from_the_table_once() {
        let keys = leader_keys();
        for pair in [
            ("h/l", "focus"),
            ("j/k", "focus"),
            ("%", "split right"),
            ("\"", "split down"),
            ("o", "next pane"),
            ("[", "copy mode"),
            ("c", "clear history"),
            ("d", "quit"),
            ("t", "keymaps"),
        ] {
            assert!(keys.contains(&pair), "{pair:?} is missing from {keys:?}");
        }
        assert_eq!(keys.len(), 9, "one entry per binding, got {keys:?}");
        // h/l and the arrows share one binding; it must not repeat.
        assert_eq!(
            keys.iter().filter(|(k, _)| *k == "h/l").count(),
            1,
            "got {keys:?}"
        );
    }

    #[test]
    fn leader_then_t_toggles_the_hint() {
        assert!(matches!(
            leader_then(KeyCode::Char('t'), KeyModifiers::NONE),
            Some(Action::ToggleHint)
        ));
    }

    #[test]
    fn uppercase_shifted_leader_key_disarms_without_action() {
        // SHIFT is accepted on plain chars; uppercase H is an unmapped
        // leader key and disarms silently rather than typing into a pane.
        let mut armed = false;
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            &Mode::Input,
            false,
            0,
        );
        assert!(
            handle(
                key(KeyCode::Char('H'), KeyModifiers::SHIFT),
                &mut armed,
                &Mode::Input,
                false,
                0
            )
            .is_none(),
            "uppercase H must not focus or send"
        );
        assert!(!armed);
    }

    fn copy_action(code: KeyCode, mods: KeyModifiers) -> Action {
        let mut armed = false;
        handle(key(code, mods), &mut armed, &Mode::Copy, false, 12)
            .unwrap_or_else(|| panic!("{code:?} {mods:?} produced no action in copy mode"))
    }

    fn select_action(code: KeyCode, mods: KeyModifiers, mode: Mode) -> Option<Action> {
        let mut armed = false;
        handle(key(code, mods), &mut armed, &mode, false, 12)
    }

    #[test]
    fn leader_bracket_enters_copy_mode_from_input() {
        assert!(matches!(
            leader_then(KeyCode::Char('['), KeyModifiers::NONE),
            Some(Action::EnterCopy)
        ));
    }

    #[test]
    fn copy_mode_j_and_k_move_the_cursor() {
        assert_eq!(
            copy_action(KeyCode::Char('j'), KeyModifiers::NONE),
            Action::CopyCursorMove { drow: 1, dcol: 0 }
        );
        assert_eq!(
            copy_action(KeyCode::Char('k'), KeyModifiers::NONE),
            Action::CopyCursorMove { drow: -1, dcol: 0 }
        );
        assert_eq!(
            copy_action(KeyCode::Char('h'), KeyModifiers::NONE),
            Action::CopyCursorMove { drow: 0, dcol: -1 }
        );
        assert_eq!(
            copy_action(KeyCode::Char('l'), KeyModifiers::NONE),
            Action::CopyCursorMove { drow: 0, dcol: 1 }
        );
    }

    #[test]
    fn copy_mode_arrows_move_the_cursor_like_hjkl() {
        assert_eq!(
            copy_action(KeyCode::Down, KeyModifiers::NONE),
            Action::CopyCursorMove { drow: 1, dcol: 0 }
        );
        assert_eq!(
            copy_action(KeyCode::Up, KeyModifiers::NONE),
            Action::CopyCursorMove { drow: -1, dcol: 0 }
        );
        assert_eq!(
            copy_action(KeyCode::Left, KeyModifiers::NONE),
            Action::CopyCursorMove { drow: 0, dcol: -1 }
        );
        assert_eq!(
            copy_action(KeyCode::Right, KeyModifiers::NONE),
            Action::CopyCursorMove { drow: 0, dcol: 1 }
        );
    }

    #[test]
    fn leader_arms_in_copy_mode_and_clear_history_works() {
        // tmux allows the prefix inside copy mode; Ctrl+a c (clear
        // history) must land from here.
        let mut armed = false;
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            &Mode::Copy,
            false,
            0,
        );
        assert!(armed, "copy mode must arm the leader");
        assert!(matches!(
            handle(
                key(KeyCode::Char('c'), KeyModifiers::NONE),
                &mut armed,
                &Mode::Copy,
                false,
                0
            ),
            Some(Action::ClearHistory)
        ));
        assert!(!armed);
        // An unknown leader key disarms and swallows.
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            &Mode::Copy,
            false,
            0,
        );
        assert!(
            handle(
                key(KeyCode::Char('z'), KeyModifiers::NONE),
                &mut armed,
                &Mode::Copy,
                false,
                0
            )
            .is_none()
        );
        assert!(!armed);
    }

    #[test]
    fn copy_mode_ctrl_d_u_scroll_half_the_pane() {
        // Ctrl+d = down the buffer (later lines, positive).
        assert_eq!(
            copy_action(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Action::CopyScroll(ScrollTarget::Delta(12))
        );
        assert_eq!(
            copy_action(KeyCode::Char('u'), KeyModifiers::CONTROL),
            Action::CopyScroll(ScrollTarget::Delta(-12))
        );
    }

    #[test]
    fn copy_mode_g_and_g_jump_to_top_and_bottom() {
        assert_eq!(
            copy_action(KeyCode::Char('g'), KeyModifiers::NONE),
            Action::CopyScroll(ScrollTarget::Top)
        );
        assert_eq!(
            copy_action(KeyCode::Char('G'), KeyModifiers::NONE),
            Action::CopyScroll(ScrollTarget::Bottom)
        );
    }

    #[test]
    fn copy_mode_v_and_v_begin_selection() {
        assert_eq!(
            copy_action(KeyCode::Char('v'), KeyModifiers::NONE),
            Action::BeginSelect(SelectMode::Span)
        );
        assert_eq!(
            copy_action(KeyCode::Char('V'), KeyModifiers::NONE),
            Action::BeginSelect(SelectMode::Rect)
        );
    }

    #[test]
    fn select_mode_motions_move_the_cursor() {
        let mode = Mode::Select(SelectMode::Span);
        assert_eq!(
            select_action(KeyCode::Char('j'), KeyModifiers::NONE, mode),
            Some(Action::SelectMove { drow: 1, dcol: 0 })
        );
        assert_eq!(
            select_action(KeyCode::Char('k'), KeyModifiers::NONE, mode),
            Some(Action::SelectMove { drow: -1, dcol: 0 })
        );
        assert_eq!(
            select_action(KeyCode::Char('h'), KeyModifiers::NONE, mode),
            Some(Action::SelectMove { drow: 0, dcol: -1 })
        );
        assert_eq!(
            select_action(KeyCode::Char('l'), KeyModifiers::NONE, mode),
            Some(Action::SelectMove { drow: 0, dcol: 1 })
        );
        assert_eq!(
            select_action(KeyCode::Left, KeyModifiers::NONE, mode),
            Some(Action::SelectMove { drow: 0, dcol: -1 })
        );
        assert_eq!(
            select_action(KeyCode::Right, KeyModifiers::NONE, mode),
            Some(Action::SelectMove { drow: 0, dcol: 1 })
        );
    }

    #[test]
    fn select_mode_y_and_enter_yank() {
        let mode = Mode::Select(SelectMode::Rect);
        assert_eq!(
            select_action(KeyCode::Char('y'), KeyModifiers::NONE, mode),
            Some(Action::Yank)
        );
        assert_eq!(
            select_action(KeyCode::Enter, KeyModifiers::NONE, mode),
            Some(Action::Yank)
        );
    }

    #[test]
    fn select_mode_v_esc_cancel_and_q_exit() {
        let mode = Mode::Select(SelectMode::Span);
        assert_eq!(
            select_action(KeyCode::Char('v'), KeyModifiers::NONE, mode),
            Some(Action::CancelSelect)
        );
        assert_eq!(
            select_action(KeyCode::Char('V'), KeyModifiers::NONE, mode),
            Some(Action::CancelSelect)
        );
        assert_eq!(
            select_action(KeyCode::Esc, KeyModifiers::NONE, mode),
            Some(Action::CancelSelect)
        );
        assert_eq!(
            select_action(KeyCode::Char('q'), KeyModifiers::NONE, mode),
            Some(Action::ExitCopy)
        );
    }

    #[test]
    fn copy_mode_q_and_esc_exit() {
        let mut armed = false;
        assert!(matches!(
            handle(
                key(KeyCode::Char('q'), KeyModifiers::NONE),
                &mut armed,
                &Mode::Copy,
                false,
                0
            ),
            Some(Action::ExitCopy)
        ));
        assert!(matches!(
            handle(
                key(KeyCode::Esc, KeyModifiers::NONE),
                &mut armed,
                &Mode::Copy,
                false,
                0
            ),
            Some(Action::ExitCopy)
        ));
    }

    #[test]
    fn copy_mode_swallows_typing_and_focus_keys() {
        for (code, mods) in [
            (KeyCode::Char('x'), KeyModifiers::NONE),
            (KeyCode::Char('a'), KeyModifiers::CONTROL),
            (KeyCode::Char('h'), KeyModifiers::NONE),
            (KeyCode::Char('s'), KeyModifiers::NONE),
            (KeyCode::Enter, KeyModifiers::NONE),
            (KeyCode::Tab, KeyModifiers::NONE),
        ] {
            let mut armed = false;
            let got = handle(key(code, mods), &mut armed, &Mode::Copy, false, 0);
            assert!(
                !matches!(got, Some(Action::Send(_))),
                "{code:?} leaked bytes to the pane in copy mode: {got:?}"
            );
            assert!(
                !matches!(got, Some(Action::Focus(_)) | Some(Action::Split(_))),
                "{code:?} changed layout in copy mode: {got:?}"
            );
        }
    }

    #[test]
    fn copy_mode_swallows_the_leader_itself() {
        // Ctrl+a in copy mode arms the leader (tmux-style) but sends
        // nothing; Ctrl+a Ctrl+a does not pass a literal 0x01 from copy
        // mode because nothing there is ever forwarded to the pane.
        let mut armed = false;
        assert!(
            handle(
                key(KeyCode::Char('a'), KeyModifiers::CONTROL),
                &mut armed,
                &Mode::Copy,
                false,
                0
            )
            .is_none()
        );
        assert!(armed, "copy mode did not arm the leader");
        assert!(
            handle(
                key(KeyCode::Char('a'), KeyModifiers::CONTROL),
                &mut armed,
                &Mode::Copy,
                false,
                0
            )
            .is_none(),
            "leader-leader in copy mode must not send bytes"
        );
    }

    #[test]
    fn copy_mode_slash_and_question_open_search_n_n_repeat() {
        assert_eq!(
            copy_action(KeyCode::Char('/'), KeyModifiers::NONE),
            Action::BeginSearch
        );
        assert_eq!(
            copy_action(KeyCode::Char('?'), KeyModifiers::SHIFT),
            Action::BeginSearchReverse
        );
        assert_eq!(
            copy_action(KeyCode::Char('n'), KeyModifiers::NONE),
            Action::SearchNext
        );
        assert_eq!(
            copy_action(KeyCode::Char('N'), KeyModifiers::NONE),
            Action::SearchPrev
        );
    }

    #[test]
    fn copy_mode_braces_jump_prompts() {
        assert_eq!(
            copy_action(KeyCode::Char('{'), KeyModifiers::NONE),
            Action::PromptPrev
        );
        assert_eq!(
            copy_action(KeyCode::Char('}'), KeyModifiers::NONE),
            Action::PromptNext
        );
    }

    fn search_action(code: KeyCode, mods: KeyModifiers) -> Action {
        let mut armed = false;
        handle(key(code, mods), &mut armed, &Mode::Search, false, 12)
            .unwrap_or_else(|| panic!("{code:?} {mods:?} produced no action in search mode"))
    }

    #[test]
    fn search_mode_types_backspace_submits_and_cancels() {
        assert_eq!(
            search_action(KeyCode::Char('e'), KeyModifiers::NONE),
            Action::SearchChar('e')
        );
        assert_eq!(
            search_action(KeyCode::Char('R'), KeyModifiers::SHIFT),
            Action::SearchChar('R')
        );
        assert_eq!(
            search_action(KeyCode::Backspace, KeyModifiers::NONE),
            Action::SearchBackspace
        );
        assert_eq!(
            search_action(KeyCode::Enter, KeyModifiers::NONE),
            Action::SearchSubmit
        );
        assert_eq!(
            search_action(KeyCode::Esc, KeyModifiers::NONE),
            Action::SearchCancel
        );
    }

    #[test]
    fn search_mode_swallows_everything_else() {
        let mut armed = false;
        for code in [KeyCode::Tab, KeyCode::F(5), KeyCode::Up] {
            assert!(
                handle(
                    key(code, KeyModifiers::NONE),
                    &mut armed,
                    &Mode::Search,
                    false,
                    0
                )
                .is_none(),
                "{code:?} produced an action in search mode"
            );
        }
        assert!(!armed, "search mode armed the leader");
    }

    fn pane_state(
        id: corral_core::tree::PaneId,
        rect: corral_core::tree::Rect,
    ) -> corrald::protocol::PaneState {
        corrald::protocol::PaneState {
            id,
            rect,
            text: String::new(),
            cursor: None,
            app_cursor: false,
            scroll: None,
            total_scrollback: 0,
            lines: vec![],
            title: String::new(),
            pwd: String::new(),
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Two panes side by side, split at column 50 (gutter at 50).
    fn two_panes() -> Vec<corrald::protocol::PaneState> {
        vec![
            pane_state(
                1,
                corral_core::tree::Rect {
                    x: 0,
                    y: 0,
                    w: 50,
                    h: 20,
                },
            ),
            pane_state(
                2,
                corral_core::tree::Rect {
                    x: 51,
                    y: 0,
                    w: 49,
                    h: 20,
                },
            ),
        ]
    }

    #[test]
    fn left_click_inside_a_pane_focuses_that_pane() {
        let panes = two_panes();
        let mut drag = None;
        let action = handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 10, 5),
            &panes,
            &Mode::Input,
            1,
            &mut drag,
        );
        assert_eq!(action, Some(MouseAction::FocusPane(1)));

        let action = handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 60, 5),
            &panes,
            &Mode::Input,
            1,
            &mut drag,
        );
        assert_eq!(action, Some(MouseAction::FocusPane(2)));
    }

    #[test]
    fn wheel_up_yields_a_scroll_delta_for_the_pane_under_the_pointer() {
        let panes = two_panes();
        let mut drag = None;
        // From input mode, the wheel enters copy mode on its way in, and
        // the pane it names is the one under the pointer, not pane 1.
        let action = handle_mouse(
            mouse(MouseEventKind::ScrollUp, 60, 5),
            &panes,
            &Mode::Input,
            2, // focused
            &mut drag,
        );
        assert_eq!(
            action,
            Some(MouseAction::EnterCopyThenScroll {
                pane: 2,
                delta: -WHEEL_DELTA,
            })
        );

        // Already in copy mode, it just scrolls.
        let action = handle_mouse(
            mouse(MouseEventKind::ScrollDown, 10, 5),
            &panes,
            &Mode::Copy,
            2, // focused
            &mut drag,
        );
        assert_eq!(
            action,
            Some(MouseAction::Scroll {
                pane: 1,
                delta: WHEEL_DELTA,
            })
        );
    }

    #[test]
    fn wheel_up_on_an_unfocused_pane_scrolls_it_without_opening_copy_mode() {
        // Copy mode belongs to the focused pane: its cursor and its yank
        // read that pane's state. Scrolling a different pane is therefore
        // a plain viewport move, not a mode change.
        let panes = two_panes();
        let mut drag = None;
        let action = handle_mouse(
            mouse(MouseEventKind::ScrollUp, 60, 5),
            &panes,
            &Mode::Input,
            1, // focused
            &mut drag,
        );
        assert_eq!(
            action,
            Some(MouseAction::Scroll {
                pane: 2,
                delta: -WHEEL_DELTA,
            })
        );
    }

    #[test]
    fn wheel_down_in_input_mode_never_opens_copy_mode() {
        // The viewport is already live in input mode, so a wheel-down has
        // nothing to reveal. Opening copy mode here is what left the
        // client stuck in it with the bar still reading copy mode.
        let panes = two_panes();
        let mut drag = None;
        let action = handle_mouse(
            mouse(MouseEventKind::ScrollDown, 10, 5),
            &panes,
            &Mode::Input,
            2, // focused
            &mut drag,
        );
        assert_eq!(
            action,
            Some(MouseAction::Scroll {
                pane: 1,
                delta: WHEEL_DELTA,
            })
        );
    }

    #[test]
    fn a_wheel_tick_outside_every_pane_does_nothing() {
        // The last row of the screen is the status bar, and the gutter is
        // between panes. Neither belongs to a pane, so there is nothing
        // to scroll.
        let panes = two_panes();
        let mut drag = None;
        for (kind, col, row) in [
            (MouseEventKind::ScrollUp, 10, 20),
            (MouseEventKind::ScrollDown, 10, 20),
            (MouseEventKind::ScrollUp, 50, 5),
        ] {
            let action = handle_mouse(mouse(kind, col, row), &panes, &Mode::Input, 1, &mut drag);
            assert_eq!(action, None, "wheel at ({col}, {row}) must do nothing");
        }
    }

    #[test]
    fn drag_on_a_gutter_between_two_panes_yields_a_ratio_change() {
        let panes = two_panes();
        let mut drag = None;
        // Mouse-down on the gutter column (50) starts tracking it.
        let down = handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 50, 5),
            &panes,
            &Mode::Input,
            2, // focused
            &mut drag,
        );
        assert_eq!(down, None, "a gutter press only arms the drag");
        assert!(drag.is_some());

        let action = handle_mouse(
            mouse(MouseEventKind::Drag(MouseButton::Left), 25, 5),
            &panes,
            &Mode::Input,
            2, // focused
            &mut drag,
        );
        match action {
            Some(MouseAction::SetSplitRatio { at, ratio }) => {
                assert_eq!(at, 1, "the ratio setter targets the left pane");
                assert!(ratio < 0.5, "dragging left of center shrinks the ratio");
            }
            other => panic!("expected a SetSplitRatio action, got {other:?}"),
        }
    }

    #[test]
    fn key_events_map_the_same_with_mouse_capture_on() {
        // Mouse support only widens the event match in main.rs's poll
        // loop; `handle` itself takes no mouse state, so a key event
        // maps exactly as it did before this task.
        let mut armed = false;
        assert_eq!(
            handle(
                key(KeyCode::Char('x'), KeyModifiers::NONE),
                &mut armed,
                &Mode::Input,
                false,
                0
            ),
            Some(Action::Send(b"x".to_vec()))
        );
    }
}
