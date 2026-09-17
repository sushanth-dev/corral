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
    /// Scroll one pane's viewport. The pane is named rather than taken
    /// from the daemon's focus: the wheel scrolls the pane under the
    /// pointer, which is not always the focused one.
    Scroll {
        pane: PaneId,
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
    /// Scroll the focused pane's viewport to the previous (up) or next
    /// (down) OSC133 prompt row (S3-8). `cursor_row` is the copy-mode
    /// cursor's row inside the viewport; the anchor is that row, not
    /// the viewport top, so a jump never skips past the prompt under
    /// the cursor. `None` anchors at the viewport bottom.
    PromptJump {
        up: bool,
        cursor_row: Option<usize>,
    },
    /// Ask what is running here. Answers `ServerMsg::WorkspaceList`.
    ListWorkspaces,
    /// A mouse click landed on this pane; focus it directly, rather than
    /// stepping there through `Focus`'s direction search.
    FocusPane {
        pane: PaneId,
    },
    /// A gutter drag moved to this position; set the ratio of the split
    /// whose immediate child is `at` (see `Node::set_ratio_near`).
    SetSplitRatio {
        at: PaneId,
        ratio: f32,
    },
}

/// One attachable workspace, as the picker shows it.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct WorkspaceInfo {
    /// Stable for the daemon's lifetime. Derived from the socket the
    /// daemon serves, so the id the user reads back names the daemon
    /// they connected to.
    pub id: String,
    pub panes: usize,
    /// How many clients are attached right now, the asking one included
    /// when it has attached. A connection that only reads a listing is
    /// not one, so the attach picker never counts itself.
    pub clients: usize,
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
    /// the search needle, plus the viewport top after the daemon
    /// scrolled to the current hit.
    SearchResult {
        pane: PaneId,
        rows: Vec<usize>,
        top: usize,
    },
    /// Reply to a prompt jump: the prompt's command text now sits at
    /// this row and column inside the viewport (not the shell theme's
    /// decorative prompt wrapper). The client moves its copy cursor
    /// there.
    PromptLanded {
        pane: PaneId,
        row: usize,
        col: usize,
    },
    /// Reply to a viewport scroll: how far the pane's viewport actually
    /// moved, in the same sign convention as `ScrollTarget::Delta`
    /// (negative is toward history, positive toward the live screen).
    /// A scroll clamps at either end of the scrollback, so this can be
    /// smaller than the requested delta, and the frame's `scroll` field
    /// cannot stand in for it: it collapses the pinned end to `None`.
    /// Copy mode moves its cursor by the full request, so it needs the
    /// difference to keep the cursor off the edge the viewport pinned
    /// against (S3-3).
    ScrollLanded {
        pane: PaneId,
        moved: isize,
    },
    /// Reply to `ClientMsg::ListWorkspaces`. One entry per workspace the
    /// daemon serves, which is one today.
    WorkspaceList {
        workspaces: Vec<WorkspaceInfo>,
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
    /// This session's viewport position inside scrollback, `None` when
    /// pinned to the bottom (live follow). The viewport belongs to the
    /// client, not the daemon: one client scrolling into history leaves
    /// every other client following the live screen. The client shows a
    /// position indicator from this and stops forwarding arrow keys to
    /// the PTY while set.
    pub scroll: Option<ScrollPos>,
    /// Total scrollback rows in the pane. The daemon clamps a session's
    /// scroll against this without a round trip to the pane worker, and
    /// it is also the screen-space row the live screen starts at. Present
    /// even while live, where `scroll` is `None` and the client still
    /// needs the range.
    pub total_scrollback: usize,
    /// The visible screen as styled runs (colors, attributes). Same row
    /// count as `text`; empty means "fall back to plain `text`".
    pub lines: Vec<StyledLine>,
    /// The pane's working directory as reported by OSC 7, decoded to a
    /// plain path, empty if never reported. The status bar shows this
    /// for the focused pane, the way tmux shows the active pane's path.
    pub pwd: String,
}

/// Scroll position of one session's viewport in one pane, mirrored from
/// `corral_core::emulation::ScrollPos`.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
pub struct ScrollPos {
    /// The viewport's top row in screen space, `0..=total`. `total` is
    /// the first row of the live screen.
    pub offset: usize,
    /// Total scrollback rows; the same value as
    /// `PaneState::total_scrollback`.
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
                pane: 1,
                target: ScrollTarget::Delta(-10),
            },
            ClientMsg::Scroll {
                pane: 2,
                target: ScrollTarget::Top,
            },
            ClientMsg::Scroll {
                pane: 2,
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
            ClientMsg::PromptJump {
                up: true,
                cursor_row: None,
            },
            ClientMsg::PromptJump {
                up: false,
                cursor_row: Some(11),
            },
            ClientMsg::FocusPane { pane: 3 },
            ClientMsg::SetSplitRatio { at: 2, ratio: 0.3 },
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
                total_scrollback: 96,
                lines: vec![],
                pwd: String::new(),
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
            let msg = ServerMsg::SearchResult {
                pane: 2,
                rows,
                top: 7,
            };
            let line = serde_json::to_string(&msg).unwrap();
            let back: ServerMsg = serde_json::from_str(&line).unwrap();
            assert_eq!(back, msg);
        }
        let empty = serde_json::to_string(&ServerMsg::SearchResult {
            pane: 2,
            rows: vec![],
            top: 0,
        })
        .unwrap();
        assert!(empty.contains("\"rows\":[]"), "got {empty}");
    }

    #[test]
    fn prompt_landed_round_trips() {
        let msg = ServerMsg::PromptLanded {
            pane: 3,
            row: 41,
            col: 6,
        };
        let line = serde_json::to_string(&msg).unwrap();
        let back: ServerMsg = serde_json::from_str(&line).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn scroll_landed_round_trips_its_signed_move() {
        // A clamped scroll toward history: the client must be able to
        // tell this from a full one.
        let msg = ServerMsg::ScrollLanded { pane: 7, moved: -5 };
        let line = serde_json::to_string(&msg).unwrap();
        let back: ServerMsg = serde_json::from_str(&line).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn list_workspaces_round_trips_with_pane_and_client_counts() {
        let msg = ClientMsg::ListWorkspaces;
        let line = serde_json::to_string(&msg).unwrap();
        let back: ClientMsg = serde_json::from_str(&line).unwrap();
        assert_eq!(back, msg);

        let reply = ServerMsg::WorkspaceList {
            workspaces: vec![
                WorkspaceInfo {
                    id: "corrald-4821".into(),
                    panes: 3,
                    clients: 2,
                },
                WorkspaceInfo {
                    id: "corrald-4822".into(),
                    panes: 0,
                    clients: 0,
                },
            ],
        };
        let line = serde_json::to_string(&reply).unwrap();
        let back: ServerMsg = serde_json::from_str(&line).unwrap();
        assert_eq!(back, reply);

        // An empty list has to be a list, not an absent field: the picker
        // reads it to decide whether it has anything to show.
        let empty =
            serde_json::to_string(&ServerMsg::WorkspaceList { workspaces: vec![] }).unwrap();
        assert!(empty.contains("\"workspaces\":[]"), "got {empty}");
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
                total_scrollback: 0,
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
                pwd: String::new(),
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
                total_scrollback: 3,
                lines: vec![],
                pwd: String::new(),
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
