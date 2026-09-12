use corral_core::tree::Dir;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub enum Action {
    Focus(Dir),
    // The plan's keymap distinguishes s (vertical) from v (horizontal);
    // the daemon applies its own geometry rule on CreatePane, so the
    // direction is carried but unused by main until per-direction splits
    // land in v0.2.
    Split(#[allow(dead_code)] Dir),
    Quit,
    Send(Vec<u8>),
}

pub fn handle(ev: KeyEvent, armed: &mut bool) -> Option<Action> {
    if *armed {
        *armed = false;
        return match ev.code {
            KeyCode::Char('h') => Some(Action::Focus(Dir::Horizontal)),
            KeyCode::Char('l') => Some(Action::Focus(Dir::Horizontal)),
            KeyCode::Char('j') => Some(Action::Focus(Dir::Vertical)),
            KeyCode::Char('k') => Some(Action::Focus(Dir::Vertical)),
            KeyCode::Char('s') => Some(Action::Split(Dir::Vertical)),
            KeyCode::Char('v') => Some(Action::Split(Dir::Horizontal)),
            KeyCode::Char('d') => Some(Action::Quit),
            _ => None,
        };
    }
    match (ev.code, ev.modifiers) {
        (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
            *armed = true;
            None
        }
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            Some(Action::Send(c.to_string().into_bytes()))
        }
        (KeyCode::Enter, _) => Some(Action::Send(b"\r".to_vec())),
        (KeyCode::Backspace, _) => Some(Action::Send(b"\x7f".to_vec())),
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(Action::Send(b"\x03".to_vec())),
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => Some(Action::Send(b"\x04".to_vec())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventState, KeyModifiers};

    fn key(code: KeyCode, mods: KeyModifiers) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent {
            code,
            modifiers: mods,
            kind: crossterm::event::KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn leader_then_h_focuses_left() {
        let mut armed = false;
        assert!(handle(key(KeyCode::Char('a'), KeyModifiers::CONTROL), &mut armed).is_none());
        assert!(armed);
        assert!(matches!(
            handle(key(KeyCode::Char('h'), KeyModifiers::NONE), &mut armed),
            Some(Action::Focus(Dir::Horizontal))
        ));
        assert!(!armed);
    }

    #[test]
    fn leader_then_j_focuses_down_and_s_v_split() {
        let mut armed = false;
        handle(key(KeyCode::Char('a'), KeyModifiers::CONTROL), &mut armed);
        assert!(matches!(
            handle(key(KeyCode::Char('j'), KeyModifiers::NONE), &mut armed),
            Some(Action::Focus(Dir::Vertical))
        ));
        handle(key(KeyCode::Char('a'), KeyModifiers::CONTROL), &mut armed);
        assert!(matches!(
            handle(key(KeyCode::Char('v'), KeyModifiers::NONE), &mut armed),
            Some(Action::Split(Dir::Horizontal))
        ));
        handle(key(KeyCode::Char('a'), KeyModifiers::CONTROL), &mut armed);
        assert!(matches!(
            handle(key(KeyCode::Char('s'), KeyModifiers::NONE), &mut armed),
            Some(Action::Split(Dir::Vertical))
        ));
    }

    #[test]
    fn leader_then_d_quits() {
        let mut armed = false;
        handle(key(KeyCode::Char('a'), KeyModifiers::CONTROL), &mut armed);
        assert!(matches!(
            handle(key(KeyCode::Char('d'), KeyModifiers::NONE), &mut armed),
            Some(Action::Quit)
        ));
        assert!(!armed);
    }

    #[test]
    fn plain_typing_sends_bytes_and_is_not_eaten() {
        let mut armed = false;
        assert!(matches!(
            handle(key(KeyCode::Char('x'), KeyModifiers::NONE), &mut armed),
            Some(Action::Send(b)) if b == b"x"
        ));
        assert!(!armed);
    }

    #[test]
    fn leader_sequence_unknown_key_disarms_and_sends_nothing() {
        let mut armed = false;
        handle(key(KeyCode::Char('a'), KeyModifiers::CONTROL), &mut armed);
        assert!(handle(key(KeyCode::Char('z'), KeyModifiers::NONE), &mut armed).is_none());
        assert!(!armed);
    }

    #[test]
    fn enter_backspace_and_ctrl_chars_route_to_the_pane() {
        let mut armed = false;
        assert!(matches!(
            handle(key(KeyCode::Enter, KeyModifiers::NONE), &mut armed),
            Some(Action::Send(b)) if b == b"\r"
        ));
        assert!(matches!(
            handle(key(KeyCode::Backspace, KeyModifiers::NONE), &mut armed),
            Some(Action::Send(b)) if b == b"\x7f"
        ));
        assert!(matches!(
            handle(key(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut armed),
            Some(Action::Send(b)) if b == b"\x03"
        ));
        assert!(matches!(
            handle(key(KeyCode::Char('d'), KeyModifiers::CONTROL), &mut armed),
            Some(Action::Send(b)) if b == b"\x04"
        ));
    }

    #[test]
    fn unhandled_keys_return_none_without_arming() {
        let mut armed = false;
        assert!(handle(key(KeyCode::F(5), KeyModifiers::NONE), &mut armed).is_none());
        assert!(handle(key(KeyCode::Char('q'), KeyModifiers::ALT), &mut armed).is_none());
        assert!(!armed);
    }
}
