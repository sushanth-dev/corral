use corral_core::tree::{Dir, PaneId, Rect};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub enum ClientMsg {
    Attach,
    CreatePane {
        cmd: String,
        args: Vec<String>,
        cwd: String,
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
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub enum ServerMsg {
    Frame {
        panes: Vec<(PaneId, Rect, String)>,
        focused: PaneId,
    },
    Exited {
        pane: PaneId,
    },
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
            },
            ClientMsg::Key {
                bytes: vec![b'x', 0x0d],
            },
            ClientMsg::Resize { cols: 80, rows: 24 },
            ClientMsg::Focus {
                dir: Dir::Horizontal,
            },
        ];
        for msg in msgs {
            let line = serde_json::to_string(&msg).unwrap();
            let back: ClientMsg = serde_json::from_str(&line).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), line);
        }
    }

    #[test]
    fn server_frame_serializes_panes_as_triples() {
        let msg = ServerMsg::Frame {
            panes: vec![(
                1,
                Rect {
                    x: 0,
                    y: 0,
                    w: 50,
                    h: 24,
                },
                "text".into(),
            )],
            focused: 1,
        };
        let line = serde_json::to_string(&msg).unwrap();
        assert!(line.contains("\"panes\":[[1,"));
        assert!(line.contains("\"focused\":1"));
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
    fn unknown_client_variant_is_rejected() {
        assert!(serde_json::from_str::<ClientMsg>("{\"Nope\":1}").is_err());
    }
}
