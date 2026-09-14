use crate::pane::{PaneCmd, PaneOut, PaneWorker};
use crate::protocol::{ClientMsg, PaneState, ServerMsg};
use anyhow::Result;
use corral_core::emulation::ScrollTarget;
use corral_core::tree::{Node, PaneId, Rect};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

const DRAIN_SWEEP: Duration = Duration::from_millis(16);

pub struct Daemon {
    root: Node,
    /// Command senders to the per-pane workers; each worker owns its
    /// pane's emulator and PTY (the emulators are !Send).
    panes: HashMap<PaneId, Sender<PaneCmd>>,
    /// Latest snapshot per pane, updated from PaneOut messages.
    snapshots: HashMap<PaneId, PaneState>,
    out_tx: Sender<PaneOut>,
    out_rx: Receiver<PaneOut>,
    focused: PaneId,
    next_id: PaneId,
    cols: u16,
    rows: u16,
}

// The daemon core no longer touches emulators or PTYs: every pane state
// mutation happens on the pane's worker thread and reaches the core as
// a PaneOut snapshot on the shared channel.
impl Daemon {
    pub fn new(cols: u16, rows: u16) -> Self {
        let (out_tx, out_rx) = channel();
        Self {
            root: Node::leaf(0),
            panes: HashMap::new(),
            snapshots: HashMap::new(),
            out_tx,
            out_rx,
            focused: 0,
            next_id: 1,
            cols,
            rows,
        }
    }

    /// Serve clients forever. The daemon state (panes, scrollback,
    /// focus) lives here, not in the connection: a disconnect (Ctrl+a d)
    /// keeps every pane running, and the next connection attaches to the
    /// same session. One client at a time; the next connection queues
    /// until the current one hangs up. The caller owns the listener and
    /// socket cleanup.
    pub fn serve(listener: UnixListener) -> Result<()> {
        let mut daemon = Daemon::new(80, 24);
        loop {
            let (stream, _) = listener.accept()?;
            daemon.run(stream)?;
        }
    }

    fn run(&mut self, stream: UnixStream) -> Result<()> {
        let mut writer = stream.try_clone()?;
        stream.set_nonblocking(true)?;
        let mut reader = BufReader::new(stream);
        // read_line on a nonblocking stream can return a partial JSON
        // line; bytes stay in this buffer until a newline completes them.
        let mut buf = String::new();
        // A re-attaching client needs the current layout immediately:
        // without panes it creates the first shell, with panes it
        // attaches to the existing ones. Push the frame before the loop
        // so the decision is frame-driven, not guessed.
        self.push_frame(&mut writer)?;
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
                // Focus, Resize, and CreatePane change layout or focus
                // without touching pane output; the client must still see
                // their effect. Key presses produce output that the drain
                // sweep pushes, so they get no extra frame here.
                if !matches!(msg, ClientMsg::Key { .. }) {
                    self.push_frame(&mut writer)?;
                }
            }
        }
        Ok(())
    }

    fn handle(&mut self, msg: &ClientMsg) -> Result<()> {
        match msg {
            ClientMsg::Attach => {}
            ClientMsg::CreatePane {
                cmd,
                args,
                cwd,
                dir,
            } => {
                let id = self.next_id;
                self.next_id += 1;
                let shell_args: Vec<&str> = args.iter().map(String::as_str).collect();
                let pty = crate::pty::PtyHandle::spawn(
                    cmd,
                    &shell_args,
                    std::path::Path::new(cwd),
                    self.cols,
                    self.rows,
                )?;
                let tx = PaneWorker::spawn(id, pty, self.cols, self.rows, self.out_tx.clone());
                self.panes.insert(id, tx);
                if self.panes.len() == 1 {
                    self.root = Node::leaf(id);
                } else {
                    // Split the focused pane in place along the direction
                    // the client asked for (Ctrl+a s vertical, Ctrl+a v
                    // horizontal). The new pane takes the sibling half.
                    let focused = Node::leaf(self.focused);
                    self.root.replace(
                        self.focused,
                        Node::split(*dir, 0.5, Box::new(focused), Box::new(Node::leaf(id))),
                    );
                }
                self.focused = id;
                // The tree changed shape: every pane's rect shrank to
                // make room. Reflow each pane's PTY and emulator so
                // output wraps at the pane width, not the old full
                // width. Covers the first pane too, which the client
                // resized before this CreatePane landed.
                let rects = self.rects();
                for (id, rect) in &rects {
                    if let Some(pane) = self.panes.get(id) {
                        pane.send(PaneCmd::Resize(rect.w, rect.h))?;
                    }
                }
            }
            ClientMsg::Key { bytes } => {
                if let Some(pane) = self.panes.get(&self.focused) {
                    pane.send(PaneCmd::Key(bytes.clone()))?;
                }
            }
            ClientMsg::Resize { cols, rows } => {
                self.cols = *cols;
                self.rows = *rows;
                let rects = self.rects();
                for (id, rect) in &rects {
                    if let Some(pane) = self.panes.get(id) {
                        pane.send(PaneCmd::Resize(rect.w, rect.h))?;
                    }
                }
            }
            ClientMsg::Focus { dir } => {
                if let Some(next) = self.root.focus_dir(self.focused, *dir) {
                    self.focused = next;
                }
            }
            ClientMsg::FocusNext => self.focus_next(),
            ClientMsg::Scroll { target } => {
                if let Some(pane) = self.panes.get(&self.focused) {
                    let target = match target {
                        crate::protocol::ScrollTarget::Delta(d) => ScrollTarget::Delta(*d),
                        crate::protocol::ScrollTarget::Row(r) => ScrollTarget::Row(*r),
                        crate::protocol::ScrollTarget::Top => ScrollTarget::Top,
                        crate::protocol::ScrollTarget::Bottom => ScrollTarget::Bottom,
                    };
                    pane.send(PaneCmd::Scroll(target))?;
                }
            }
            ClientMsg::Search {
                needle,
                from,
                reverse,
            } => {
                if let Some(pane) = self.panes.get(&self.focused) {
                    pane.send(PaneCmd::Search {
                        needle: needle.clone(),
                        from: *from,
                        reverse: *reverse,
                    })?;
                }
            }
            ClientMsg::ClearHistory => {
                if let Some(pane) = self.panes.get(&self.focused) {
                    pane.send(PaneCmd::ClearHistory)?;
                }
            }
            ClientMsg::DumpScrollback { pane } => {
                let target = pane.unwrap_or(self.focused);
                if let Some(p) = self.panes.get(&target) {
                    p.send(PaneCmd::DumpScrollback)?;
                }
            }
            ClientMsg::PromptJump { up, cursor_row } => {
                if let Some(pane) = self.panes.get(&self.focused) {
                    pane.send(PaneCmd::PromptJump {
                        up: *up,
                        cursor_row: *cursor_row,
                    })?;
                }
            }
            ClientMsg::YankCommand { anchor } => {
                if let Some(pane) = self.panes.get(&self.focused) {
                    pane.send(PaneCmd::YankCommand { anchor: *anchor })?;
                }
            }
            ClientMsg::LoadScrollback { text } => {
                if let Some(pane) = self.panes.get(&self.focused) {
                    pane.send(PaneCmd::LoadScrollback { text: text.clone() })?;
                }
            }
        }
        Ok(())
    }

    /// Cycle focus through the panes in tree order, wrapping at the end.
    fn focus_next(&mut self) {
        let ids = self.root.leaf_ids();
        if ids.len() < 2 {
            return;
        }
        let pos = ids.iter().position(|&id| id == self.focused);
        let next = match pos {
            Some(p) => ids[(p + 1) % ids.len()],
            None => ids[0],
        };
        self.focused = next;
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
            let wait = DRAIN_SWEEP.min(deadline.saturating_duration_since(Instant::now()));
            match self.out_rx.recv_timeout(wait) {
                Ok(PaneOut::Snapshot { pane, state }) => {
                    self.snapshots.insert(pane, state);
                    changed = true;
                }
                Ok(PaneOut::SearchResult { pane, rows, top }) => {
                    // A search reply goes straight to the client, outside
                    // the normal frame cadence.
                    write_msg(writer, &ServerMsg::SearchResult { pane, rows, top })?;
                }
                Ok(PaneOut::ScrollbackDump { pane, text }) => {
                    // Same out-of-band path as the search reply (S3-7).
                    write_msg(writer, &ServerMsg::ScrollbackDump { pane, text })?;
                }
                Ok(PaneOut::PromptLanded { pane, row, col }) => {
                    // The client moves its copy cursor onto the command.
                    write_msg(writer, &ServerMsg::PromptLanded { pane, row, col })?;
                }
                Ok(PaneOut::Exited { pane }) => {
                    // The worker also reports Exited when its command
                    // channel drops at daemon shutdown; only a live pane
                    // collapses the tree.
                    if self.panes.contains_key(&pane) {
                        exited.push(pane);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                // Every worker is gone: nothing left to drain.
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        // Snapshots first, frame while the panes are still alive: the
        // client must see a pane's final output before the Exited
        // message collapses the tree.
        if changed || !exited.is_empty() {
            self.push_frame(writer)?;
        }
        for id in &exited {
            self.panes.remove(id);
            self.snapshots.remove(id);
            let sibling = self.root.remove(*id);
            if let Some(sib) = sibling {
                self.focused = sib;
            }
            write_msg(writer, &ServerMsg::Exited { pane: *id })?;
        }
        if !exited.is_empty() {
            // The tree collapsed: surviving panes' rects grew to fill
            // the closed pane's space. Reflow each PTY and emulator to
            // the new size, same as CreatePane's split does, so the
            // remaining panes unwrap back to full width.
            let rects = self.rects();
            for (id, rect) in &rects {
                if let Some(pane) = self.panes.get(id) {
                    pane.send(PaneCmd::Resize(rect.w, rect.h))?;
                }
            }
            self.push_frame(writer)?;
        }
        Ok(())
    }

    fn push_frame(&self, writer: &mut UnixStream) -> Result<()> {
        let rects = self.rects();
        let mut panes = Vec::new();
        for (id, rect) in &rects {
            let Some(snapshot) = self.snapshots.get(id) else {
                continue;
            };
            let mut state = snapshot.clone();
            state.rect = *rect;
            panes.push(state);
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
    write_all_blocking(writer, line.as_bytes())
}

// The client stream is nonblocking; a full socket buffer returns
// WouldBlock (EAGAIN), which is retryable, not fatal. Spin with a short
// sleep until the kernel takes the rest. Without this, a large styled
// frame kills the daemon when the socket buffer fills.
fn write_all_blocking(writer: &mut UnixStream, mut bytes: &[u8]) -> Result<()> {
    while !bytes.is_empty() {
        match writer.write(bytes) {
            Ok(0) => {
                return Err(
                    std::io::Error::new(std::io::ErrorKind::WriteZero, "zero write").into(),
                );
            }
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(e) => return Err(e.into()),
        }
    }
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
    use corral_core::tree::Dir;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    fn send(stream: &mut UnixStream, msg: &ClientMsg) {
        let mut line = serde_json::to_string(msg).unwrap();
        line.push('\n');
        stream.write_all(line.as_bytes()).unwrap();
        stream.flush().unwrap();
    }

    /// Binds a listener in a fresh temp dir and serves it on a detached
    /// thread. The daemon never stops (a persistent session outlives any
    /// client); tests just need the socket path.
    fn start_daemon(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("corral-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || Daemon::serve(listener).unwrap());
        sock
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
        std::thread::spawn(move || Daemon::serve(listener).unwrap());

        let mut client = UnixStream::connect(&sock).unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "printf hello".into()],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
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
        drop(reader); // serve ends when the client drops
        let _ = std::fs::remove_file(dir.join("s.sock"));
    }

    #[test]
    fn two_panes_render_side_by_side() {
        let dir = std::env::temp_dir().join(format!("corral-test-two-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || Daemon::serve(listener).unwrap());

        let mut client = UnixStream::connect(&sock).unwrap();
        for out in ["one", "two"] {
            send(
                &mut client,
                &ClientMsg::CreatePane {
                    cmd: "sh".into(),
                    args: vec!["-c".into(), format!("printf {out}; sleep 2")],
                    cwd: "/tmp".into(),
                    dir: Dir::Horizontal,
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
        let _ = std::fs::remove_file(dir.join("s.sock"));
    }

    #[test]
    fn focus_message_moves_focus_in_frames() {
        let dir = std::env::temp_dir().join(format!("corral-test-focus-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || Daemon::serve(listener).unwrap());

        let mut client = UnixStream::connect(&sock).unwrap();
        for out in ["one", "two"] {
            send(
                &mut client,
                &ClientMsg::CreatePane {
                    cmd: "sh".into(),
                    args: vec!["-c".into(), format!("printf {out}; sleep 2")],
                    cwd: "/tmp".into(),
                    dir: Dir::Horizontal,
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
        let _ = std::fs::remove_file(dir.join("s.sock"));
    }

    #[test]
    fn focus_pushes_a_frame_even_when_panes_produce_no_output() {
        // Regression: Focus (and Resize) change no pane output, and the
        // daemon only pushed frames when output changed, so switching
        // panes never reached the client. Quiet panes (sleep) must not
        // swallow the frame.
        let sock = start_daemon("focus-quiet");
        let mut client = UnixStream::connect(&sock).unwrap();
        for _ in ["a", "b"] {
            send(
                &mut client,
                &ClientMsg::CreatePane {
                    cmd: "sh".into(),
                    args: vec!["-c".into(), "sleep 30".into()],
                    cwd: "/tmp".into(),
                    dir: Dir::Horizontal,
                },
            );
        }
        let mut reader = BufReader::new(client);
        let frame = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.len() == 2,
            _ => false,
        })
        .expect("two quiet panes must still produce a frame");
        let ServerMsg::Frame {
            focused: focused_before,
            ..
        } = frame
        else {
            unreachable!()
        };
        send(
            reader.get_mut(),
            &ClientMsg::Focus {
                dir: Dir::Horizontal,
            },
        );
        let frame = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { focused, .. } => *focused != focused_before,
            _ => false,
        })
        .expect("focus change must push a frame for quiet panes");
        let ServerMsg::Frame { focused, .. } = frame else {
            unreachable!()
        };
        assert_ne!(focused, focused_before);
        drop(reader);
        let _ = std::fs::remove_file(sock);
    }

    #[test]
    fn resize_message_resizes_the_emulators() {
        let dir = std::env::temp_dir().join(format!("corral-test-resize-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || Daemon::serve(listener).unwrap());

        let mut client = UnixStream::connect(&sock).unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "printf hi".into()],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
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
        let _ = std::fs::remove_file(dir.join("s.sock"));
    }

    #[test]
    fn exited_message_arrives_when_the_shell_quits() {
        let dir = std::env::temp_dir().join(format!("corral-test-exit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || Daemon::serve(listener).unwrap());

        let mut client = UnixStream::connect(&sock).unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "printf bye".into()],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
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
        let _ = std::fs::remove_file(dir.join("s.sock"));
    }

    #[test]
    fn keys_route_to_the_focused_pane_only() {
        // Two panes run `cat`; typing lands only in the focused (second)
        // pane, and the first pane never receives the bytes.
        let _sock = start_daemon("keys");
        let mut client = UnixStream::connect(&_sock).unwrap();
        for out in ["one", "two"] {
            send(
                &mut client,
                &ClientMsg::CreatePane {
                    cmd: "sh".into(),
                    args: vec!["-c".into(), format!("printf {out}; cat")],
                    cwd: "/tmp".into(),
                    dir: Dir::Horizontal,
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
    }

    #[test]
    fn second_exit_leaves_a_single_collapsed_pane() {
        // Three short-lived panes: each exit collapses the tree until one
        // pane fills the whole frame.
        let _sock = start_daemon("collapse");
        let mut client = UnixStream::connect(&_sock).unwrap();
        for out in ["one", "two", "three"] {
            send(
                &mut client,
                &ClientMsg::CreatePane {
                    cmd: "sh".into(),
                    args: vec!["-c".into(), format!("printf {out}")],
                    cwd: "/tmp".into(),
                    dir: Dir::Horizontal,
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
    }

    #[test]
    fn surviving_pane_unwraps_to_fill_the_closed_sibling_space() {
        // A pane that closes must free its space back to its sibling, and
        // the sibling's PTY must actually resize to it, not just the
        // frame's rect metadata: `stty size` reports what the pane's own
        // shell sees, so a stale PTY size shows up here even though the
        // rect already looks right.
        let sock = start_daemon("unwrap");
        let mut client = UnixStream::connect(&sock).unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec![
                    "-c".into(),
                    "while true; do stty size; sleep 0.05; done".into(),
                ],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
            },
        );
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                // A pane that exits instantly races the daemon's own
                // collapse-and-reflow: it can vanish before any frame
                // ever shows the split (narrowed) state. Stay alive
                // briefly so the split is actually observable first.
                args: vec!["-c".into(), "sleep 0.5; printf closing".into()],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
            },
        );
        let mut reader = BufReader::new(client);
        // The loop's very first stty line can still show the pre-split
        // width (spawned at 80 before CreatePane's reflow lands), so
        // read the most recent line in the pane, not the first.
        fn last_stty_cols(text: &str) -> Option<u32> {
            text.lines()
                .rev()
                .find_map(|l| l.split_whitespace().nth(1)?.parse().ok())
        }
        let split = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes
                .iter()
                .any(|p| last_stty_cols(&p.text).is_some_and(|cols| cols < 80)),
            _ => false,
        })
        .expect("split frame with narrowed stty output within 5s");
        let ServerMsg::Frame { panes, .. } = split else {
            unreachable!()
        };
        let narrow_pane = panes
            .iter()
            .find(|p| last_stty_cols(&p.text).is_some_and(|cols| cols < 80))
            .expect("stty size line narrower than 80");
        let narrow_id = narrow_pane.id;

        // The second pane exits; its space must unwrap back onto the
        // first, and the first pane's PTY must actually widen to match.
        let unwrapped = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.len() == 1,
            _ => false,
        })
        .expect("collapsed frame with one pane within 5s");
        let ServerMsg::Frame { panes, .. } = unwrapped else {
            unreachable!()
        };
        assert_eq!(
            panes[0].id, narrow_id,
            "the surviving pane is the stty loop"
        );

        let wide = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => {
                panes.iter().any(|p| last_stty_cols(&p.text) == Some(80))
            }
            _ => false,
        })
        .expect("stty size reports the full 80 cols after unwrap within 5s");
        let ServerMsg::Frame { panes, .. } = wide else {
            unreachable!()
        };
        assert_eq!(
            last_stty_cols(&panes[0].text),
            Some(80),
            "surviving pane's PTY must actually resize to the freed width"
        );
        drop(reader);
    }

    #[test]
    fn garbage_lines_between_messages_are_skipped() {
        // The daemon must not die on malformed JSON from a client.
        let _sock = start_daemon("garbage");
        let mut client = UnixStream::connect(&_sock).unwrap();
        client.write_all(b"not json at all\n{\"Torn\":\n").unwrap();
        client.flush().unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "printf fine".into()],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
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
    }

    #[test]
    fn resize_to_tiny_then_back_restores_layout() {
        let _sock = start_daemon("tiny");
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
                    dir: Dir::Horizontal,
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
    }

    #[test]
    fn second_pane_splits_the_focused_pane_not_the_screen() {
        // Split-in-place: pane 2 must take half of pane 1's rect, leaving
        // a nested layout, not two half-screen panes.
        let _sock = start_daemon("split");
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
                dir: Dir::Horizontal,
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
                dir: Dir::Horizontal,
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
    }

    #[test]
    fn vertical_split_request_splits_top_bottom() {
        // The client's s key asks for Vertical; the daemon must honor the
        // requested direction, not pick one from pane geometry.
        let _sock = start_daemon("vsplit");
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
                dir: Dir::Horizontal,
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
                dir: Dir::Vertical,
            },
        );
        let frame = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => {
                panes.len() == 2 && panes.iter().any(|p| p.text.contains("two"))
            }
            _ => false,
        })
        .expect("vertical split frame");
        let ServerMsg::Frame { panes, .. } = frame else {
            unreachable!()
        };
        assert_eq!(panes.len(), 2);
        // Vertical: each half is about 20 tall of 40, full 100 wide.
        for p in &panes {
            assert!(
                p.rect.h <= 21 && p.rect.h >= 19,
                "vertical split rect {:?} should be half of 40 tall",
                p.rect
            );
            assert_eq!(p.rect.w, 100, "vertical split keeps full width");
        }
        drop(reader);
        drop(client);
    }

    #[test]
    fn panes_survive_a_disconnect_and_a_second_client_reattaches() {
        // Ctrl+a d must not kill the session: the daemon keeps panes
        // running after the client drops, and the next connection sees
        // the existing pane in its first frame.
        let _sock = start_daemon("detach");
        let mut first = UnixStream::connect(&_sock).unwrap();
        send(
            &mut first,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "printf kept; cat".into()],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
            },
        );
        let mut reader = BufReader::new(first.try_clone().unwrap());
        wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.text.contains("kept")),
            _ => false,
        })
        .expect("first client sees the pane");
        drop(reader);
        drop(first);

        // The same daemon serves the next connection with the live pane.
        let second = UnixStream::connect(&_sock).unwrap();
        let mut reader = BufReader::new(second);
        let frame = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.text.contains("kept")),
            _ => false,
        })
        .expect("re-attach frame carries the surviving pane");
        let ServerMsg::Frame { panes, focused } = frame else {
            unreachable!()
        };
        assert_eq!(panes.len(), 1, "the pane survived the disconnect");
        assert_eq!(focused, panes[0].id);

        // The surviving pane still answers input from the new client.
        send(
            reader.get_mut(),
            &ClientMsg::Key {
                bytes: b"alive\n".to_vec(),
            },
        );
        wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.text.contains("alive")),
            _ => false,
        })
        .expect("reattached pane accepts keys");
    }

    #[test]
    fn attach_pushes_an_immediate_frame_and_keeps_the_connection() {
        let _sock = start_daemon("attach");
        let mut client = UnixStream::connect(&_sock).unwrap();
        send(&mut client, &ClientMsg::Attach);
        // The connection-start frame arrives first (empty at this point);
        // a pane created after Attach works normally.
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "printf later".into()],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
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
    }

    #[test]
    fn focus_into_an_empty_direction_keeps_current_focus() {
        // A single pane: focus down/left has no neighbor, so the focused
        // id in subsequent frames must stay on the live pane.
        let _sock = start_daemon("nofocus");
        let mut client = UnixStream::connect(&_sock).unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "printf solo; sleep 2".into()],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
            },
        );
        let mut reader = BufReader::new(client);
        // The connect-time frame is empty; focus is meaningful once the
        // pane exists.
        let first = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => !panes.is_empty(),
            _ => false,
        })
        .expect("first frame with the pane");
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

    #[test]
    fn scroll_command_moves_the_viewport_and_reports_position() {
        let sock = start_daemon("scroll");
        let mut client = UnixStream::connect(&sock).unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "seq 1 60; sleep 30".into()],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
            },
        );
        let mut reader = BufReader::new(client);
        // Pinned to the bottom: scroll is None and the last line shows.
        let bottom = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes
                .iter()
                .any(|p| p.text.contains("60") && p.scroll.is_none()),
            _ => false,
        })
        .expect("bottom frame with line 60 and scroll None within 5s");
        let ServerMsg::Frame { panes, .. } = bottom else {
            unreachable!()
        };
        let bottom_text = panes[0].text.clone();
        let last = bottom_text
            .lines()
            .rev()
            .find(|l| !l.is_empty())
            .expect("bottom frame shows at least one line");
        assert!(last.contains("60"), "last line {last:?} must be 60");

        send(
            reader.get_mut(),
            &ClientMsg::Scroll {
                target: crate::protocol::ScrollTarget::Delta(-10),
            },
        );
        let scrolled = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.scroll.is_some()),
            _ => false,
        })
        .expect("scrolled frame with a scroll position within 5s");
        let ServerMsg::Frame { panes, .. } = scrolled else {
            unreachable!()
        };
        let pos = panes[0].scroll.expect("scroll populated after Delta(-10)");
        assert!(pos.offset > 0, "offset must be positive after scrolling up");
        assert!(
            !panes[0].text.contains("60"),
            "bottom line must not show while scrolled up, got {:?}",
            panes[0].text
        );

        send(
            reader.get_mut(),
            &ClientMsg::Scroll {
                target: crate::protocol::ScrollTarget::Bottom,
            },
        );
        let restored = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.scroll.is_none()),
            _ => false,
        })
        .expect("restored frame with scroll None within 5s");
        let ServerMsg::Frame { panes, .. } = restored else {
            unreachable!()
        };
        assert!(
            panes[0].text.contains("60"),
            "bottom shows the latest line again, got {:?}",
            panes[0].text
        );
        drop(reader);
    }

    #[test]
    fn search_finds_rows_and_the_viewport_jumps_to_the_first_hit() {
        let sock = start_daemon("search");
        let mut client = UnixStream::connect(&sock).unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec![
                    "-c".into(),
                    "for i in 1 2 3; do echo MARKER line $i; done; seq 1 60; sleep 30".into(),
                ],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
            },
        );
        let mut reader = BufReader::new(client);
        wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.text.contains("60")),
            _ => false,
        })
        .expect("bottom frame showing line 60 within 5s");

        // Forward search from the top: three hits, and the viewport jumps
        // to the first one so the marker is on screen.
        send(
            reader.get_mut(),
            &ClientMsg::Search {
                needle: "MARKER".into(),
                from: None,
                reverse: false,
            },
        );
        let reply = wait_for_msg(&mut reader, |m| matches!(m, ServerMsg::SearchResult { .. }))
            .expect("SearchResult within 5s");
        let ServerMsg::SearchResult { pane, rows, top } = reply else {
            unreachable!()
        };
        assert_eq!(rows.len(), 3, "three marker lines, got {rows:?}");
        assert!(pane > 0, "reply names the pane it searched");
        assert_eq!(
            top, rows[0],
            "reported viewport top must match the row the daemon scrolled to"
        );

        let jumped = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes
                .iter()
                .any(|p| p.scroll.is_some() && p.text.contains("MARKER")),
            _ => false,
        })
        .expect("frame scrolled to the first marker within 5s");
        let ServerMsg::Frame { panes, .. } = jumped else {
            unreachable!()
        };
        let pos = panes[0].scroll.expect("scroll set after search jump");
        assert!(
            panes[0].text.contains("MARKER"),
            "viewport shows the match, got {:?}",
            panes[0].text
        );

        // n resumes past the viewport top: the second marker, not the
        // same one again.
        send(
            reader.get_mut(),
            &ClientMsg::Search {
                needle: "MARKER".into(),
                from: Some(pos.offset),
                reverse: false,
            },
        );
        let again = wait_for_msg(&mut reader, |m| matches!(m, ServerMsg::SearchResult { .. }))
            .expect("second SearchResult within 5s");
        let ServerMsg::SearchResult {
            rows: next_rows, ..
        } = again
        else {
            unreachable!()
        };
        assert!(
            next_rows.first().copied() > rows.first().copied(),
            "resume must skip the first hit, got {next_rows:?} after {rows:?}"
        );
        drop(reader);
    }

    #[test]
    fn clear_history_empties_scrollback_but_leaves_the_live_screen_alone() {
        let sock = start_daemon("clear");
        let mut client = UnixStream::connect(&sock).unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "seq 1 60; sleep 30".into()],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
            },
        );
        let mut reader = BufReader::new(client);
        let bottom = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.text.contains("60")),
            _ => false,
        })
        .expect("bottom frame showing line 60 within 5s");
        let ServerMsg::Frame { panes, .. } = bottom else {
            unreachable!()
        };
        let live_text = panes[0].text.clone();

        // Scrolled up first so the viewport is not pinned to the bottom;
        // the clear only touches scrollback, not the copy-mode position.
        send(
            reader.get_mut(),
            &ClientMsg::Scroll {
                target: crate::protocol::ScrollTarget::Top,
            },
        );
        wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.scroll.is_some()),
            _ => false,
        })
        .expect("frame scrolled away from the bottom within 5s");

        send(reader.get_mut(), &ClientMsg::ClearHistory);
        // Scrolling up now has nothing to reach: with the scrollback gone,
        // any further Top scroll lands right back on the live screen.
        // That is the proof the scrollback (not the screen) was cleared.
        send(
            reader.get_mut(),
            &ClientMsg::Scroll {
                target: crate::protocol::ScrollTarget::Top,
            },
        );
        let settled = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.text == live_text),
            _ => false,
        })
        .expect("frame back on the live screen within 5s");
        let ServerMsg::Frame { panes, .. } = settled else {
            unreachable!()
        };
        assert_eq!(
            panes[0].text, live_text,
            "the live screen survives ClearHistory unchanged"
        );
        drop(reader);
    }

    #[test]
    fn dump_scrollback_returns_all_sixty_lines() {
        let sock = start_daemon("dump");
        let mut client = UnixStream::connect(&sock).unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "seq 1 60; sleep 30".into()],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
            },
        );
        let mut reader = BufReader::new(client);
        wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.text.contains("60")),
            _ => false,
        })
        .expect("bottom frame showing line 60 within 5s");

        send(reader.get_mut(), &ClientMsg::DumpScrollback { pane: None });
        let reply = wait_for_msg(&mut reader, |m| {
            matches!(m, ServerMsg::ScrollbackDump { .. })
        })
        .expect("ScrollbackDump within 5s");
        let ServerMsg::ScrollbackDump { pane, text } = reply else {
            unreachable!()
        };
        assert!(pane > 0, "reply names the pane it dumped");
        let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines.len(),
            60,
            "all 60 lines are in the dump, got {}",
            lines.len()
        );
        assert_eq!(lines.first().copied(), Some("1"), "dump starts at line 1");
        assert_eq!(lines.last().copied(), Some("60"), "dump ends at line 60");
        drop(reader);
    }

    #[test]
    fn prompt_jump_scrolls_between_osc133_prompts() {
        let sock = start_daemon("promptjump");
        let mut client = UnixStream::connect(&sock).unwrap();
        // Three literal OSC133 A markers over a 60-line pane. `sh` does
        // not emit these itself; fish would (plan Task S3-8 guardrail).
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec![
                    "-c".into(),
                    "printf '\\033]133;A\\033\\\\'; echo prompt one; seq 1 20; printf '\\033]133;A\\033\\\\'; echo prompt two; seq 21 40; printf '\\033]133;A\\033\\\\'; echo prompt three; seq 41 100; sleep 30".into(),
                ],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
            },
        );
        let mut reader = BufReader::new(client);
        wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes
                .iter()
                .any(|p| p.text.contains("100") && p.scroll.is_none()),
            _ => false,
        })
        .expect("bottom frame showing line 100 within 5s");

        // Up jumps to the previous prompt row. From the bottom the first
        // up-jump reaches the bottom-most prompt (three); walking up
        // again reaches two, then one.
        send(
            reader.get_mut(),
            &ClientMsg::PromptJump {
                up: true,
                cursor_row: None,
            },
        );
        let landed = wait_for_msg(&mut reader, |m| matches!(m, ServerMsg::PromptLanded { .. }))
            .expect("PromptLanded reply within 5s");
        let ServerMsg::PromptLanded { pane, row, col } = landed else {
            unreachable!()
        };
        assert!(
            pane > 0 && row < 24,
            "landing row is viewport-relative: {row}"
        );
        assert_eq!(
            col, 0,
            "sh never emits OSC 133;B, so the fallback lands at column 0"
        );
        wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes
                .iter()
                .any(|p| p.scroll.is_some() && p.text.contains("prompt three")),
            _ => false,
        })
        .expect("frame showing prompt three after up-jump within 5s");

        // The client replays its cursor row from the landing reply: the
        // anchor is the cursor, not the viewport top, so the walk moves
        // one prompt per jump instead of re-finding the same one.
        let mut cursor_row = row;
        for (needle, label) in [
            ("prompt two", "second up-jump"),
            ("prompt one", "third up-jump"),
        ] {
            send(
                reader.get_mut(),
                &ClientMsg::PromptJump {
                    up: true,
                    cursor_row: Some(cursor_row),
                },
            );
            // The reply precedes the frame in the same drain sweep, so
            // read it first; the frame-wait below would swallow it.
            let landed = wait_for_msg(&mut reader, |m| matches!(m, ServerMsg::PromptLanded { .. }))
                .expect("PromptLanded reply within 5s");
            let ServerMsg::PromptLanded { row: r, .. } = landed else {
                unreachable!()
            };
            cursor_row = r;
            wait_for_msg(&mut reader, |m| match m {
                ServerMsg::Frame { panes, .. } => panes
                    .iter()
                    .any(|p| p.scroll.is_some() && p.text.contains(needle)),
                _ => false,
            })
            .unwrap_or_else(|| panic!("frame showing {needle} after {label} within 5s"));
        }

        // Down returns to the next prompt.
        send(
            reader.get_mut(),
            &ClientMsg::PromptJump {
                up: false,
                cursor_row: Some(cursor_row),
            },
        );
        wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes
                .iter()
                .any(|p| p.scroll.is_some() && p.text.contains("prompt two")),
            _ => false,
        })
        .expect("frame showing prompt two after down-jump within 5s");

        // `c` yanks the current command's block: from the bottom, the
        // third prompt through its last output row.
        send(reader.get_mut(), &ClientMsg::YankCommand { anchor: None });
        let yank = wait_for_msg(&mut reader, |m| {
            matches!(m, ServerMsg::ScrollbackDump { .. })
        })
        .expect("ScrollbackDump for yank within 5s");
        let ServerMsg::ScrollbackDump { text, .. } = yank else {
            unreachable!()
        };
        assert!(text.contains("prompt three"), "got {text:?}");
        assert!(
            text.contains("100"),
            "last output row in the block, got {text:?}"
        );
        assert!(
            !text.contains("prompt two") && !text.contains("prompt one"),
            "block stops at the next prompt, got {text:?}"
        );
        drop(reader);
    }
}
