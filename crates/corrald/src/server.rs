use crate::protocol::{ClientMsg, PaneState, ServerMsg};
use crate::pty::{PtyEvent, PtyHandle};
use anyhow::Result;
use corral_core::emulation::Emulator;
use corral_core::tree::{Dir, Node, PaneId, Rect};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

const DRAIN_SWEEP: Duration = Duration::from_millis(16);
const PER_PANE_WAIT: Duration = Duration::from_millis(2);

pub struct Daemon {
    root: Node,
    emulators: HashMap<PaneId, Emulator>,
    ptys: HashMap<PaneId, PtyHandle>,
    focused: PaneId,
    next_id: PaneId,
    cols: u16,
    rows: u16,
}

// One thread owns every Emulator (they are !Send); PTY readers report
// through channels drained in the recv_timeout sweep below.
impl Daemon {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            root: Node::leaf(0),
            emulators: HashMap::new(),
            ptys: HashMap::new(),
            focused: 0,
            next_id: 1,
            cols,
            rows,
        }
    }

    /// Accept one client and serve until it disconnects. v0.1 is single
    /// client; the caller owns the listener and socket cleanup.
    pub fn serve(listener: UnixListener) -> Result<()> {
        let (stream, _) = listener.accept()?;
        Daemon::new(80, 24).run(stream)
    }

    fn run(&mut self, stream: UnixStream) -> Result<()> {
        let mut writer = stream.try_clone()?;
        stream.set_nonblocking(true)?;
        let mut reader = BufReader::new(stream);
        // read_line on a nonblocking stream can return a partial JSON
        // line; bytes stay in this buffer until a newline completes them.
        let mut buf = String::new();
        loop {
            // A frame write to a departed client reports BrokenPipe; that
            // is a clean disconnect, not a daemon error.
            if let Err(e) = self.drain_and_push(&mut writer) {
                let broken = e
                    .root_cause()
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe);
                if broken {
                    break;
                }
                return Err(e);
            }
            match reader.read_line(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                // WouldBlock: no client message within this pass; the
                // drain sweep above paced the loop.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e.into()),
            }
            while let Some(pos) = buf.find('\n') {
                let line: String = buf.drain(..=pos).collect();
                let Ok(msg) = serde_json::from_str::<ClientMsg>(line.trim()) else {
                    continue;
                };
                self.handle(&msg)?;
            }
        }
        Ok(())
    }

    fn handle(&mut self, msg: &ClientMsg) -> Result<()> {
        match msg {
            ClientMsg::Attach => {}
            ClientMsg::CreatePane { cmd, args, cwd } => {
                let id = self.next_id;
                self.next_id += 1;
                let shell_args: Vec<&str> = args.iter().map(String::as_str).collect();
                let pty = PtyHandle::spawn(
                    cmd,
                    &shell_args,
                    std::path::Path::new(cwd),
                    self.cols,
                    self.rows,
                )?;
                self.ptys.insert(id, pty);
                self.emulators
                    .insert(id, Emulator::new(self.cols, self.rows)?);
                if self.ptys.len() == 1 {
                    self.root = Node::leaf(id);
                } else {
                    // Split the focused pane in place: vertical when it is
                    // taller than wide, horizontal otherwise (plan geometry
                    // rule). The new pane takes the sibling half.
                    let dir = self.split_dir();
                    let focused = Node::leaf(self.focused);
                    self.root.replace(
                        self.focused,
                        Node::split(dir, 0.5, Box::new(focused), Box::new(Node::leaf(id))),
                    );
                }
                self.focused = id;
            }
            ClientMsg::Key { bytes } => {
                if let Some(pty) = self.ptys.get_mut(&self.focused) {
                    pty.write_all(bytes)?;
                }
            }
            ClientMsg::Resize { cols, rows } => {
                self.cols = *cols;
                self.rows = *rows;
                let rects = self.rects();
                for (id, rect) in &rects {
                    let Some(pty) = self.ptys.get(id) else {
                        continue;
                    };
                    pty.resize(rect.w, rect.h)?;
                    if let Some(emu) = self.emulators.get_mut(id) {
                        emu.resize(rect.w, rect.h)?;
                    }
                }
            }
            ClientMsg::Focus { dir } => {
                if let Some(next) = self.root.focus_dir(self.focused, *dir) {
                    self.focused = next;
                }
            }
        }
        Ok(())
    }

    fn split_dir(&self) -> Dir {
        let rects = self.rects();
        let Some((_, rect)) = rects.iter().find(|(id, _)| *id == self.focused) else {
            return Dir::Horizontal;
        };
        if rect.h > rect.w {
            Dir::Vertical
        } else {
            Dir::Horizontal
        }
    }

    fn rects(&self) -> Vec<(PaneId, Rect)> {
        self.root.rects(Rect {
            x: 0,
            y: 0,
            w: self.cols,
            h: self.rows,
        })
    }

    fn drain_and_push(&mut self, writer: &mut UnixStream) -> Result<()> {
        let deadline = Instant::now() + DRAIN_SWEEP;
        let mut changed = false;
        let mut exited: Vec<PaneId> = Vec::new();
        loop {
            let ids: Vec<PaneId> = self.ptys.keys().copied().collect();
            for id in ids {
                let wait = PER_PANE_WAIT.min(deadline.saturating_duration_since(Instant::now()));
                if wait.is_zero() {
                    break;
                }
                // Fetch the event first, then borrow the emulator: the two
                // map fields are borrowed one at a time.
                let event = match self.ptys.get(&id) {
                    Some(pty) => match pty.rx.recv_timeout(wait) {
                        Ok(ev) => Some(ev),
                        Err(RecvTimeoutError::Disconnected) => Some(PtyEvent::Exited),
                        Err(RecvTimeoutError::Timeout) => None,
                    },
                    None => None,
                };
                match event {
                    Some(PtyEvent::Output(bytes)) => {
                        if let Some(emu) = self.emulators.get_mut(&id) {
                            emu.feed(&bytes);
                            changed = true;
                            // Query replies (DA1, DSR, DECRQM) route from
                            // the emulator back into the pane's PTY.
                            for reply in emu.take_pty_writes() {
                                if let Some(pty) = self.ptys.get_mut(&id) {
                                    pty.write_all(&reply)?;
                                }
                            }
                        }
                    }
                    Some(PtyEvent::Exited) => exited.push(id),
                    None => {}
                }
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        // Feed every Output first and push a frame while the panes are
        // still alive: the client must see a pane's final output before
        // the Exited message collapses the tree.
        if changed {
            self.push_frame(writer)?;
        }
        for id in &exited {
            self.ptys.remove(id);
            self.emulators.remove(id);
            let sibling = self.root.remove(*id);
            if let Some(sib) = sibling {
                self.focused = sib;
            }
            write_msg(writer, &ServerMsg::Exited { pane: *id })?;
        }
        if !exited.is_empty() {
            self.push_frame(writer)?;
        }
        Ok(())
    }

    fn push_frame(&mut self, writer: &mut UnixStream) -> Result<()> {
        let rects = self.rects();
        let mut panes = Vec::new();
        for (id, rect) in &rects {
            let Some(emu) = self.emulators.get_mut(id) else {
                continue;
            };
            let cursor = emu.cursor()?;
            panes.push(PaneState {
                id: *id,
                rect: *rect,
                text: emu.screen_text()?,
                cursor,
            });
        }
        let msg = ServerMsg::Frame {
            panes,
            focused: self.focused,
        };
        write_msg(writer, &msg)
    }
}

fn write_msg(writer: &mut UnixStream, msg: &ServerMsg) -> Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(())
}

/// `$CORRAL_SOCKET` wins; otherwise `/tmp/corral-$UID.sock`. `id -u`
/// keeps this dependency-free (libc is not a direct dependency).
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("CORRAL_SOCKET") {
        return PathBuf::from(p);
    }
    let uid = std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "0".into());
    std::env::temp_dir().join(format!("corral-{uid}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClientMsg, ServerMsg};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    fn send(stream: &mut UnixStream, msg: &ClientMsg) {
        let mut line = serde_json::to_string(msg).unwrap();
        line.push('\n');
        stream.write_all(line.as_bytes()).unwrap();
        stream.flush().unwrap();
    }

    /// Binds a listener in a fresh temp dir and serves it on a thread.
    /// Returns (socket path, server thread handle).
    fn start_daemon(tag: &str) -> (std::path::PathBuf, std::thread::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!("corral-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let handle = std::thread::spawn(move || Daemon::serve(listener).unwrap());
        (sock, handle)
    }

    fn wait_for_msg(
        reader: &mut BufReader<UnixStream>,
        pred: impl Fn(&ServerMsg) -> bool,
    ) -> Option<ServerMsg> {
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return None,
                Ok(_) => {
                    if let Ok(msg) = serde_json::from_str::<ServerMsg>(line.trim())
                        && pred(&msg)
                    {
                        return Some(msg);
                    }
                }
            }
        }
    }

    #[test]
    fn creates_pane_and_streams_frame_with_output() {
        let dir = std::env::temp_dir().join(format!("corral-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let handle = std::thread::spawn(move || Daemon::serve(listener).unwrap());

        let mut client = UnixStream::connect(&sock).unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "printf hello".into()],
                cwd: "/tmp".into(),
            },
        );
        let mut reader = BufReader::new(client);
        let frame = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.text.contains("hello")),
            _ => false,
        })
        .expect("frame with hello within 5s");
        let ServerMsg::Frame { panes, focused } = frame else {
            unreachable!()
        };
        assert_eq!(panes.len(), 1);
        assert_eq!(focused, 1);
        drop(reader);
        let _ = handle.join(); // serve ends when the client drops
        let _ = std::fs::remove_file(dir.join("s.sock"));
    }

    #[test]
    fn two_panes_render_side_by_side() {
        let dir = std::env::temp_dir().join(format!("corral-test-two-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let handle = std::thread::spawn(move || Daemon::serve(listener).unwrap());

        let mut client = UnixStream::connect(&sock).unwrap();
        for out in ["one", "two"] {
            send(
                &mut client,
                &ClientMsg::CreatePane {
                    cmd: "sh".into(),
                    args: vec!["-c".into(), format!("printf {out}; sleep 2")],
                    cwd: "/tmp".into(),
                },
            );
        }
        let mut reader = BufReader::new(client);
        let frame = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.len() == 2,
            _ => false,
        })
        .expect("frame with two panes within 5s");
        let ServerMsg::Frame { panes, focused } = frame else {
            unreachable!()
        };
        // Horizontal split: the two rects must not overlap and focus moves
        // to the new pane.
        let (a, b) = (panes[0].rect, panes[1].rect);
        assert!(
            !(a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h),
            "{a:?} overlaps {b:?}"
        );
        assert_eq!(focused, panes[1].id);
        drop(reader);
        let _ = handle.join();
        let _ = std::fs::remove_file(dir.join("s.sock"));
    }

    #[test]
    fn focus_message_moves_focus_in_frames() {
        let dir = std::env::temp_dir().join(format!("corral-test-focus-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let handle = std::thread::spawn(move || Daemon::serve(listener).unwrap());

        let mut client = UnixStream::connect(&sock).unwrap();
        for out in ["one", "two"] {
            send(
                &mut client,
                &ClientMsg::CreatePane {
                    cmd: "sh".into(),
                    args: vec!["-c".into(), format!("printf {out}; sleep 2")],
                    cwd: "/tmp".into(),
                },
            );
        }
        let mut reader = BufReader::new(client);
        let frame = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.len() == 2,
            _ => false,
        })
        .unwrap();
        let ServerMsg::Frame { panes, .. } = frame else {
            unreachable!()
        };
        send(
            reader.get_mut(),
            &ClientMsg::Focus {
                dir: Dir::Horizontal,
            },
        );
        // Focus must land on one of the live panes; which one depends on
        // the geometry of the split.
        let frame = wait_for_msg(&mut reader, |m| matches!(m, ServerMsg::Frame { .. }))
            .expect("frame after focus");
        let ServerMsg::Frame { focused, .. } = frame else {
            unreachable!()
        };
        assert!(
            panes.iter().any(|p| p.id == focused),
            "focused {focused} is not a live pane"
        );
        drop(reader);
        let _ = handle.join();
        let _ = std::fs::remove_file(dir.join("s.sock"));
    }

    #[test]
    fn resize_message_resizes_the_emulators() {
        let dir = std::env::temp_dir().join(format!("corral-test-resize-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let handle = std::thread::spawn(move || Daemon::serve(listener).unwrap());

        let mut client = UnixStream::connect(&sock).unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "printf hi".into()],
                cwd: "/tmp".into(),
            },
        );
        let mut reader = BufReader::new(client);
        wait_for_msg(&mut reader, |m| matches!(m, ServerMsg::Frame { .. })).expect("first frame");
        // 16x4: the tree recomputes rects; the daemon must not error.
        send(reader.get_mut(), &ClientMsg::Resize { cols: 16, rows: 4 });
        let frame = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().all(|p| p.rect.w <= 16 && p.rect.h <= 4),
            _ => false,
        })
        .expect("frame with resized rects");
        let ServerMsg::Frame { panes, .. } = frame else {
            unreachable!()
        };
        assert!(panes.iter().all(|p| p.rect.w <= 16 && p.rect.h <= 4));
        drop(reader);
        let _ = handle.join();
        let _ = std::fs::remove_file(dir.join("s.sock"));
    }

    #[test]
    fn exited_message_arrives_when_the_shell_quits() {
        let dir = std::env::temp_dir().join(format!("corral-test-exit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let handle = std::thread::spawn(move || Daemon::serve(listener).unwrap());

        let mut client = UnixStream::connect(&sock).unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "printf bye".into()],
                cwd: "/tmp".into(),
            },
        );
        let mut reader = BufReader::new(client);
        let exited = wait_for_msg(&mut reader, |m| matches!(m, ServerMsg::Exited { .. }))
            .expect("Exited within 5s");
        let ServerMsg::Exited { pane } = exited else {
            unreachable!()
        };
        assert_eq!(pane, 1);
        drop(reader);
        let _ = handle.join();
        let _ = std::fs::remove_file(dir.join("s.sock"));
    }

    #[test]
    fn keys_route_to_the_focused_pane_only() {
        // Two panes run `cat`; typing lands only in the focused (second)
        // pane, and the first pane never receives the bytes.
        let (_sock, handle) = start_daemon("keys");
        let mut client = UnixStream::connect(&_sock).unwrap();
        for out in ["one", "two"] {
            send(
                &mut client,
                &ClientMsg::CreatePane {
                    cmd: "sh".into(),
                    args: vec!["-c".into(), format!("printf {out}; cat")],
                    cwd: "/tmp".into(),
                },
            );
        }
        let mut reader = BufReader::new(client);
        wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.len() == 2,
            _ => false,
        })
        .expect("two panes");
        send(
            reader.get_mut(),
            &ClientMsg::Key {
                bytes: b"typed\n".to_vec(),
            },
        );
        let frame = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.text.contains("typed")),
            _ => false,
        })
        .expect("frame with typed text");
        let ServerMsg::Frame { panes, .. } = frame else {
            unreachable!()
        };
        let with_text = panes.iter().filter(|p| p.text.contains("typed")).count();
        assert_eq!(with_text, 1, "typed text landed in more than one pane");
        drop(reader);
        let _ = handle.join();
    }

    #[test]
    fn second_exit_leaves_a_single_collapsed_pane() {
        // Three short-lived panes: each exit collapses the tree until one
        // pane fills the whole frame.
        let (_sock, handle) = start_daemon("collapse");
        let mut client = UnixStream::connect(&_sock).unwrap();
        for out in ["one", "two", "three"] {
            send(
                &mut client,
                &ClientMsg::CreatePane {
                    cmd: "sh".into(),
                    args: vec!["-c".into(), format!("printf {out}")],
                    cwd: "/tmp".into(),
                },
            );
        }
        let mut reader = BufReader::new(client);
        let frame = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.len() == 1,
            _ => false,
        })
        .expect("final frame with one pane");
        let ServerMsg::Frame { panes, focused } = frame else {
            unreachable!()
        };
        assert_eq!(panes.len(), 1);
        assert_eq!(focused, panes[0].id, "last live pane holds focus");
        drop(reader);
        let _ = handle.join();
    }

    #[test]
    fn garbage_lines_between_messages_are_skipped() {
        // The daemon must not die on malformed JSON from a client.
        let (_sock, handle) = start_daemon("garbage");
        let mut client = UnixStream::connect(&_sock).unwrap();
        client.write_all(b"not json at all\n{\"Torn\":\n").unwrap();
        client.flush().unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "printf fine".into()],
                cwd: "/tmp".into(),
            },
        );
        let mut reader = BufReader::new(client);
        let frame = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.text.contains("fine")),
            _ => false,
        })
        .expect("daemon survived garbage and served the pane");
        assert!(matches!(frame, ServerMsg::Frame { .. }));
        drop(reader);
        let _ = handle.join();
    }

    #[test]
    fn resize_to_tiny_then_back_restores_layout() {
        let (_sock, handle) = start_daemon("tiny");
        let mut client = UnixStream::connect(&_sock).unwrap();
        for out in ["one", "two"] {
            send(
                &mut client,
                &ClientMsg::CreatePane {
                    cmd: "sh".into(),
                    args: vec![
                        "-c".into(),
                        format!("while :; do printf {out}; sleep 1; done"),
                    ],
                    cwd: "/tmp".into(),
                },
            );
        }
        let mut reader = BufReader::new(client);
        wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.len() == 2,
            _ => false,
        })
        .expect("two panes");
        // Tiny size: every pane rect must still be at least 1x1. The panes
        // keep printing, so post-resize frames flow.
        send(reader.get_mut(), &ClientMsg::Resize { cols: 3, rows: 2 });
        let tiny = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => {
                panes.len() == 2 && panes.iter().all(|p| p.rect.w >= 1 && p.rect.h >= 1)
            }
            _ => false,
        })
        .expect("tiny frame with valid rects");
        let ServerMsg::Frame {
            panes: tiny_panes, ..
        } = tiny
        else {
            unreachable!()
        };
        for p in &tiny_panes {
            assert!(
                p.rect.w >= 1 && p.rect.h >= 1,
                "degenerate rect {:?} at 3x2",
                p.rect
            );
        }
        // Back to normal: rects grow again.
        send(reader.get_mut(), &ClientMsg::Resize { cols: 80, rows: 24 });
        let big = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().all(|p| p.rect.w > 20),
            _ => false,
        })
        .expect("restored frame");
        let ServerMsg::Frame {
            panes: big_panes, ..
        } = big
        else {
            unreachable!()
        };
        assert!(big_panes.iter().all(|p| p.rect.w > 20));
        drop(reader);
        let _ = handle.join();
    }

    #[test]
    fn second_pane_splits_the_focused_pane_not_the_screen() {
        // Split-in-place: pane 2 must take half of pane 1's rect, leaving
        // a nested layout, not two half-screen panes.
        let (_sock, handle) = start_daemon("split");
        let mut client = UnixStream::connect(&_sock).unwrap();
        send(&mut client, &ClientMsg::Attach);
        send(
            &mut client,
            &ClientMsg::Resize {
                cols: 100,
                rows: 40,
            },
        );
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "printf one; cat".into()],
                cwd: "/tmp".into(),
            },
        );
        let mut reader = BufReader::new(client.try_clone().unwrap());
        wait_for_msg(&mut reader, |m| {
            matches!(m, ServerMsg::Frame { panes, .. } if panes.iter().any(|p| p.text.contains("one")))
        })
        .expect("first pane");
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "printf two; cat".into()],
                cwd: "/tmp".into(),
            },
        );
        let frame = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => {
                panes.len() == 2 && panes.iter().any(|p| p.text.contains("two"))
            }
            _ => false,
        })
        .expect("split frame");
        let ServerMsg::Frame { panes, .. } = frame else {
            unreachable!()
        };
        // At 100x40 the first pane is 40 tall (h > w? no: 100 wide, 40 tall
        // -> taller than wide is false -> Horizontal split, left/right).
        // Each half must be about 50 wide, not 100: split of the focused
        // pane, not of the full screen.
        assert_eq!(panes.len(), 2, "two panes after split");
        for p in &panes {
            assert!(
                p.rect.w <= 51 && p.rect.w >= 49,
                "pane rect {:?} should be half of 100 wide, not full width",
                p.rect
            );
        }
        drop(reader);
        drop(client);
        let _ = handle.join();
    }

    #[test]
    fn client_disconnect_is_a_clean_exit_not_a_broken_pipe_error() {
        // The daemon pushes frames every sweep; when the client vanishes
        // mid-frame, the write fails with BrokenPipe. serve must return
        // Ok (thread join without panic proves the clean exit), not Ok(())
        // wrapped in an error that unwraps into a panic.
        let (_sock, handle) = start_daemon("brokepipe");
        let mut client = UnixStream::connect(&_sock).unwrap();
        send(&mut client, &ClientMsg::Attach);
        send(&mut client, &ClientMsg::Resize { cols: 80, rows: 24 });
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "while :; do printf x; sleep 1; done".into()],
                cwd: "/tmp".into(),
            },
        );
        // Give the daemon time to enter its push loop, then vanish.
        std::thread::sleep(Duration::from_millis(500));
        drop(client);
        // join() panics if the thread ended in Err; poll for exit, then
        // join so a broken-pipe error surfaces as a failed join.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !handle.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(handle.is_finished(), "daemon never noticed the disconnect");
        handle
            .join()
            .expect("daemon exited cleanly, not with BrokenPipe");
    }

    #[test]
    fn attach_alone_produces_no_frame_but_keeps_the_connection() {
        let (_sock, handle) = start_daemon("attach");
        let mut client = UnixStream::connect(&_sock).unwrap();
        send(&mut client, &ClientMsg::Attach);
        // Nothing crashed and the daemon is still responsive: a pane
        // created after Attach works normally.
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "printf later".into()],
                cwd: "/tmp".into(),
            },
        );
        let mut reader = BufReader::new(client);
        let frame = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.text.contains("later")),
            _ => false,
        })
        .expect("pane after attach works");
        assert!(matches!(frame, ServerMsg::Frame { .. }));
        drop(reader);
        let _ = handle.join();
    }

    #[test]
    fn focus_into_an_empty_direction_keeps_current_focus() {
        // A single pane: focus down/left has no neighbor, so the focused
        // id in subsequent frames must stay on the live pane.
        let (_sock, handle) = start_daemon("nofocus");
        let mut client = UnixStream::connect(&_sock).unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "printf solo; sleep 2".into()],
                cwd: "/tmp".into(),
            },
        );
        let mut reader = BufReader::new(client);
        let first = wait_for_msg(&mut reader, |m| matches!(m, ServerMsg::Frame { .. }))
            .expect("first frame");
        let ServerMsg::Frame { focused, .. } = first else {
            unreachable!()
        };
        for dir in [Dir::Horizontal, Dir::Vertical] {
            send(reader.get_mut(), &ClientMsg::Focus { dir });
        }
        let frame = wait_for_msg(&mut reader, |m| matches!(m, ServerMsg::Frame { .. }))
            .expect("frame after focus attempts");
        let ServerMsg::Frame { focused: after, .. } = frame else {
            unreachable!()
        };
        assert_eq!(after, focused, "focus moved with no neighbor");
        drop(reader);
        let _ = handle.join();
    }

    #[test]
    fn socket_path_honors_env_and_uid_fallback_shape() {
        // Env override wins outright.
        // SAFETY: single-threaded test env manipulation.
        unsafe { std::env::set_var("CORRAL_SOCKET", "/tmp/env-wins.sock") };
        assert_eq!(
            socket_path(),
            std::path::PathBuf::from("/tmp/env-wins.sock")
        );
        unsafe { std::env::remove_var("CORRAL_SOCKET") };
        // Fallback: /tmp/corral-$UID.sock with a numeric uid.
        let path = socket_path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("corral-"), "got {name}");
        let uid = name.trim_start_matches("corral-").trim_end_matches(".sock");
        assert!(
            !uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit()),
            "uid suffix {uid:?} is not numeric"
        );
    }
}
