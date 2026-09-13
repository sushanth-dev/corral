use corral_core::tree::{Dir, PaneId, Rect};
use serde::{Deserialize, Serialize};

/// Where to scroll a pane's viewport. Mirrors
/// `corral_core::emulation::ScrollTarget` over the wire.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
pub enum ScrollTarget {
    Delta(isize),
    /// Absolute screen-space row from the top of scrollback.
    Row(usize),
    Top,
    Bottom,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub enum ClientMsg {
    Attach,
    CreatePane {
        cmd: String,
        args: Vec<String>,
        cwd: String,
        dir: Dir,
    },
    Key {
        bytes: Vec<u8>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Focus {
        dir: Dir,
    },
    Scroll {
        target: ScrollTarget,
    },
    Search {
        needle: String,
        /// Screen-space row to resume after (forward) or before
        /// (reverse); `None` starts from the top or bottom.
        from: Option<usize>,
        reverse: bool,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub enum ServerMsg {
    Frame {
        panes: Vec<PaneState>,
        focused: PaneId,
    },
    Exited {
        pane: PaneId,
    },
    /// Rows (screen space) in the requested pane whose text contains
    /// the search needle.
    SearchResult {
        pane: PaneId,
        rows: Vec<usize>,
    },
}

/// One pane's render state. The cursor is None while a full-screen
/// program (alternate screen) owns the pane or the cursor sits outside
/// the viewport.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct PaneState {
    pub id: PaneId,
    pub rect: Rect,
    pub text: String,
    pub cursor: Option<(u16, u16)>,
    /// The pane requested application cursor keys (DECCKM); the client
    /// then sends arrows as ESC O A..D instead of ESC [ A..D.
    pub app_cursor: bool,
    /// The viewport's position inside scrollback, `None` when pinned to
    /// the bottom (live follow). The client shows a position indicator
    /// from this and stops forwarding arrow keys to the PTY while set.
    pub scroll: Option<ScrollPos>,
}

/// Scroll position of one pane's viewport, mirrored from
/// `corral_core::emulation::ScrollPos`.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
pub struct ScrollPos {
    /// Rows the viewport top is above the bottom of the active screen.
    pub offset: usize,
    /// Total scrollback rows.
    pub total: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use corral_core::tree::Rect;

    #[test]
    fn client_messages_round_trip_through_json() {
        let msgs = vec![
            ClientMsg::Attach,
            ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "printf hello".into()],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
            },
            ClientMsg::Key {
                bytes: vec![b'x', 0x0d],
            },
            ClientMsg::Resize { cols: 80, rows: 24 },
            ClientMsg::Focus {
                dir: Dir::Horizontal,
            },
            ClientMsg::Scroll {
                target: ScrollTarget::Delta(-10),
            },
            ClientMsg::Scroll {
                target: ScrollTarget::Top,
            },
            ClientMsg::Scroll {
                target: ScrollTarget::Bottom,
            },
            ClientMsg::Search {
                needle: "make build".into(),
                from: None,
                reverse: false,
            },
            ClientMsg::Search {
                needle: "error".into(),
                from: Some(41),
                reverse: true,
            },
        ];
        for msg in msgs {
            let line = serde_json::to_string(&msg).unwrap();
            let back: ClientMsg = serde_json::from_str(&line).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), line);
        }
    }

    #[test]
    fn server_frame_serializes_pane_states() {
        let msg = ServerMsg::Frame {
            panes: vec![PaneState {
                id: 1,
                rect: Rect {
                    x: 0,
                    y: 0,
                    w: 50,
                    h: 24,
                },
                text: "text".into(),
                cursor: Some((10, 3)),
                app_cursor: false,
                scroll: Some(ScrollPos {
                    offset: 12,
                    total: 96,
                }),
            }],
            focused: 1,
        };
        let line = serde_json::to_string(&msg).unwrap();
        assert!(line.contains("\"panes\":[{\"id\":1,"));
        assert!(line.contains("\"focused\":1"));
        assert!(
            line.contains("\"scroll\":{\"offset\":12,\"total\":96}"),
            "scroll position must ride on the pane state: {line}"
        );
        let back: ServerMsg = serde_json::from_str(&line).unwrap();
        assert!(matches!(back, ServerMsg::Frame { focused: 1, .. }));
    }

    #[test]
    fn exited_carries_the_dead_pane_id() {
        let line = serde_json::to_string(&ServerMsg::Exited { pane: 4 }).unwrap();
        assert!(line.contains("\"pane\":4"));
        let back: ServerMsg = serde_json::from_str(&line).unwrap();
        assert!(matches!(back, ServerMsg::Exited { pane: 4 }));
    }

    #[test]
    fn search_result_round_trips_with_empty_rows() {
        for rows in [vec![0usize, 10, 27], vec![]] {
            let msg = ServerMsg::SearchResult { pane: 2, rows };
            let line = serde_json::to_string(&msg).unwrap();
            let back: ServerMsg = serde_json::from_str(&line).unwrap();
            assert_eq!(back, msg);
        }
        let empty = serde_json::to_string(&ServerMsg::SearchResult {
            pane: 2,
            rows: vec![],
        })
        .unwrap();
        assert!(empty.contains("\"rows\":[]"), "got {empty}");
    }

    #[test]
    fn unknown_client_variant_is_rejected() {
        assert!(serde_json::from_str::<ClientMsg>("{\"Nope\":1}").is_err());
    }

    #[test]
    fn create_pane_with_empty_args_and_cwd_round_trips() {
        let msg = ClientMsg::CreatePane {
            cmd: "/bin/sh".into(),
            args: vec![],
            cwd: String::new(),
            dir: Dir::Vertical,
        };
        let line = serde_json::to_string(&msg).unwrap();
        let back: ClientMsg = serde_json::from_str(&line).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn key_with_empty_byte_vector_is_valid() {
        let msg = ClientMsg::Key { bytes: vec![] };
        let line = serde_json::to_string(&msg).unwrap();
        assert_eq!(line, r#"{"Key":{"bytes":[]}}"#);
        let back: ClientMsg = serde_json::from_str(&line).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn resize_accepts_extreme_dimensions() {
        for (cols, rows) in [(1, 1), (u16::MAX, u16::MAX)] {
            let msg = ClientMsg::Resize { cols, rows };
            let back: ClientMsg =
                serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
            assert_eq!(back, msg);
        }
        // Overflowing u16 is rejected, not clamped.
        assert!(
            serde_json::from_str::<ClientMsg>(r#"{"Resize":{"cols":70000,"rows":24}}"#).is_err()
        );
    }

    #[test]
    fn frame_with_no_panes_round_trips() {
        let msg = ServerMsg::Frame {
            panes: vec![],
            focused: 0,
        };
        let back: ServerMsg = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn pane_text_with_unicode_and_escapes_survives_json() {
        let msg = ServerMsg::Frame {
            panes: vec![PaneState {
                id: 1,
                rect: Rect {
                    x: 0,
                    y: 0,
                    w: 80,
                    h: 24,
                },
                text: "héllo こんにちは \"quoted\" \\\nnewline".into(),
                cursor: None,
                app_cursor: true,
                scroll: None,
            }],
            focused: 1,
        };
        let back: ServerMsg = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn exited_for_pane_zero_is_representable() {
        let back: ServerMsg = serde_json::from_str(r#"{"Exited":{"pane":0}}"#).unwrap();
        assert_eq!(back, ServerMsg::Exited { pane: 0 });
    }

    #[test]
    fn truncated_json_line_is_rejected_not_panicking() {
        let full = serde_json::to_string(&ClientMsg::Attach).unwrap();
        let cut = &full[..full.len() / 2];
        assert!(serde_json::from_str::<ClientMsg>(cut).is_err());
        assert!(serde_json::from_str::<ClientMsg>("").is_err());
        assert!(serde_json::from_str::<ClientMsg>("   ").is_err());
    }

    #[test]
    fn server_message_unknown_variant_is_rejected() {
        assert!(serde_json::from_str::<ServerMsg>("{\"Whatever\":1}").is_err());
    }
}
