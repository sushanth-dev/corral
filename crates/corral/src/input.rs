use corral_core::tree::Dir;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug)]
pub enum Action {
    Focus(Dir),
    // s splits vertically, v horizontally; the daemon splits the focused
    // pane along the requested direction.
    Split(Dir),
    Quit,
    Send(Vec<u8>),
}

// The Ctrl+a leader is the only key corral consumes. Everything else
// forwards to the focused pane exactly as a plain terminal would deliver
// it: control bytes, escape sequences, modifier chords and all.

pub fn handle(ev: KeyEvent, armed: &mut bool, app_cursor: bool) -> Option<Action> {
    if *armed {
        *armed = false;
        return match (ev.code, ev.modifiers) {
            // Ctrl+a Ctrl+a passes a real 0x01 through, tmux-style.
            (KeyCode::Char('a'), KeyModifiers::CONTROL) => Some(Action::Send(vec![0x01])),
            (KeyCode::Char('h'), _) => Some(Action::Focus(Dir::Horizontal)),
            (KeyCode::Char('l'), _) => Some(Action::Focus(Dir::Horizontal)),
            (KeyCode::Char('j'), _) => Some(Action::Focus(Dir::Vertical)),
            (KeyCode::Char('k'), _) => Some(Action::Focus(Dir::Vertical)),
            (KeyCode::Char('s'), _) => Some(Action::Split(Dir::Vertical)),
            (KeyCode::Char('v'), _) => Some(Action::Split(Dir::Horizontal)),
            (KeyCode::Char('d'), _) => Some(Action::Quit),
            _ => None,
        };
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
    use crossterm::event::{KeyCode, KeyEventState};

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
        match handle(key(code, mods), &mut armed, app_cursor) {
            Some(Action::Send(b)) => b,
            other => panic!("{code:?} {mods:?} produced {other:?}, expected Send"),
        }
    }

    #[test]
    fn leader_then_h_focuses_left() {
        let mut armed = false;
        assert!(
            handle(
                key(KeyCode::Char('a'), KeyModifiers::CONTROL),
                &mut armed,
                false
            )
            .is_none()
        );
        assert!(armed);
        assert!(matches!(
            handle(
                key(KeyCode::Char('h'), KeyModifiers::NONE),
                &mut armed,
                false
            ),
            Some(Action::Focus(Dir::Horizontal))
        ));
        assert!(!armed);
    }

    #[test]
    fn leader_then_j_focuses_down_and_s_v_split() {
        let mut armed = false;
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            false,
        );
        assert!(matches!(
            handle(
                key(KeyCode::Char('j'), KeyModifiers::NONE),
                &mut armed,
                false
            ),
            Some(Action::Focus(Dir::Vertical))
        ));
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            false,
        );
        assert!(matches!(
            handle(
                key(KeyCode::Char('v'), KeyModifiers::NONE),
                &mut armed,
                false
            ),
            Some(Action::Split(Dir::Horizontal))
        ));
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            false,
        );
        assert!(matches!(
            handle(
                key(KeyCode::Char('s'), KeyModifiers::NONE),
                &mut armed,
                false
            ),
            Some(Action::Split(Dir::Vertical))
        ));
    }

    #[test]
    fn leader_then_d_quits() {
        let mut armed = false;
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            false,
        );
        assert!(matches!(
            handle(
                key(KeyCode::Char('d'), KeyModifiers::NONE),
                &mut armed,
                false
            ),
            Some(Action::Quit)
        ));
        assert!(!armed);
    }

    #[test]
    fn plain_typing_sends_bytes_and_is_not_eaten() {
        let mut armed = false;
        assert!(matches!(
            handle(key(KeyCode::Char('x'), KeyModifiers::NONE), &mut armed, false),
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
            false,
        );
        assert!(
            handle(
                key(KeyCode::Char('z'), KeyModifiers::NONE),
                &mut armed,
                false
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
            false,
        );
        assert!(armed);
        assert!(matches!(
            handle(key(KeyCode::Char('a'), KeyModifiers::CONTROL), &mut armed, false),
            Some(Action::Send(b)) if b == vec![0x01]
        ));
        assert!(!armed);
        // A fresh Ctrl+a re-arms afterwards.
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            false,
        );
        assert!(armed);
        assert!(matches!(
            handle(
                key(KeyCode::Char('d'), KeyModifiers::NONE),
                &mut armed,
                false
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
                false
            )
            .is_none()
        );
        assert!(handle(key(KeyCode::F(13), KeyModifiers::NONE), &mut armed, false).is_none());
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
            (KeyCode::Char('s'), |a: &Action| {
                matches!(a, Action::Split(Dir::Vertical))
            }),
            (KeyCode::Char('v'), |a: &Action| {
                matches!(a, Action::Split(Dir::Horizontal))
            }),
            (KeyCode::Char('d'), |a: &Action| matches!(a, Action::Quit)),
        ];
        for (code, check) in cases {
            let mut armed = false;
            handle(
                key(KeyCode::Char('a'), KeyModifiers::CONTROL),
                &mut armed,
                false,
            );
            let action = handle(key(code, KeyModifiers::NONE), &mut armed, false)
                .unwrap_or_else(|| panic!("{code:?} produced no action"));
            assert!(check(&action), "{code:?} produced {action:?}");
            assert!(!armed, "{code:?} left the leader armed");
        }
    }

    #[test]
    fn uppercase_shifted_leader_key_disarms_without_action() {
        // SHIFT is accepted on plain chars; uppercase H is an unmapped
        // leader key and disarms silently rather than typing into a pane.
        let mut armed = false;
        handle(
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut armed,
            false,
        );
        assert!(
            handle(
                key(KeyCode::Char('H'), KeyModifiers::SHIFT),
                &mut armed,
                false
            )
            .is_none(),
            "uppercase H must not focus or send"
        );
        assert!(!armed);
    }
}
