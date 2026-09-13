use corral_core::emulation::StyledLine;
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
    /// Move focus to the next pane in tree order, wrapping.
    FocusNext,
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
    /// Erase the focused pane's scrollback (CSI 3 J).
    ClearHistory,
    /// Dump a pane's full scrollback (S3-7). `None` means the focused
    /// pane; the reply rides ServerMsg::ScrollbackDump.
    DumpScrollback {
        pane: Option<PaneId>,
    },
    /// Scroll the focused pane's viewport to the previous (up) or next
    /// (down) OSC133 prompt row (S3-8). `cursor_row` is the copy-mode
    /// cursor's row inside the viewport; the anchor is that row, not
    /// the viewport top, so a jump never skips past the prompt under
    /// the cursor. `None` anchors at the viewport bottom.
    PromptJump {
        up: bool,
        cursor_row: Option<usize>,
    },
    /// Extract the current command's output (S3-8): from the prompt row
    /// above `anchor` (a screen-space row; `None` pins to the bottom) to
    /// the row before the next prompt. The text rides back on
    /// ServerMsg::ScrollbackDump.
    YankCommand {
        anchor: Option<usize>,
    },
    /// Replace a pane's scrollback with edited text (S3-7 write-back):
    /// the daemon erases the pane's history and feeds `text` through the
    /// emulator, so the terminal view reflects the editor's changes.
    LoadScrollback {
        text: String,
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
    /// The named pane's full scrollback as plain text (S3-7), one
    /// screen-space row per line.
    ScrollbackDump {
        pane: PaneId,
        text: String,
    },
    /// Reply to a prompt jump: the prompt now sits at this row inside
    /// the viewport. The client moves its copy cursor there.
    PromptLanded {
        pane: PaneId,
        row: usize,
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
    /// The visible screen as styled runs (colors, attributes). Same row
    /// count as `text`; empty means "fall back to plain `text`".
    pub lines: Vec<StyledLine>,
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
            ClientMsg::FocusNext,
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
            ClientMsg::ClearHistory,
            ClientMsg::DumpScrollback { pane: None },
            ClientMsg::DumpScrollback { pane: Some(3) },
            ClientMsg::PromptJump {
                up: true,
                cursor_row: None,
            },
            ClientMsg::PromptJump {
                up: false,
                cursor_row: Some(11),
            },
            ClientMsg::YankCommand { anchor: None },
            ClientMsg::YankCommand { anchor: Some(7) },
            ClientMsg::LoadScrollback {
                text: "edited\nlines".into(),
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
                lines: vec![],
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
    fn scrollback_dump_round_trips() {
        let msg = ServerMsg::ScrollbackDump {
            pane: 5,
            text: "line one\nline two\n".into(),
        };
        let back: ServerMsg = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn prompt_landed_round_trips() {
        let msg = ServerMsg::PromptLanded { pane: 3, row: 41 };
        let line = serde_json::to_string(&msg).unwrap();
        let back: ServerMsg = serde_json::from_str(&line).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn pane_lines_carry_styled_runs_through_json() {
        use corral_core::emulation::{CellAttrs, CellColor, StyledLine, StyledRun};
        let msg = ServerMsg::Frame {
            panes: vec![PaneState {
                id: 1,
                rect: Rect {
                    x: 0,
                    y: 0,
                    w: 80,
                    h: 3,
                },
                text: "red plain\n".into(),
                cursor: None,
                app_cursor: false,
                scroll: None,
                lines: vec![StyledLine {
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
                            text: " plain".into(),
                            fg: CellColor::Default,
                            bg: CellColor::Rgb(10, 20, 30),
                            attrs: CellAttrs::default(),
                        },
                    ],
                }],
            }],
            focused: 1,
        };
        let line = serde_json::to_string(&msg).unwrap();
        assert!(
            line.contains("\"fg\":{\"Indexed\":1}"),
            "indexed fg must survive: {line}"
        );
        assert!(
            line.contains("\"bg\":{\"Rgb\":[10,20,30]}"),
            "rgb bg must survive: {line}"
        );
        let back: ServerMsg = serde_json::from_str(&line).unwrap();
        assert_eq!(back, msg);
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
                lines: vec![],
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
