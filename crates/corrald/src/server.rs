use crate::pane::{PaneCmd, PaneOut, PaneWorker};
use crate::protocol::{ClientMsg, PaneState, ScrollPos, ServerMsg, WorkspaceInfo};
use anyhow::Result;
use corral_core::emulation::{ScrollTarget, StyledLine};
use corral_core::tree::{Node, PaneId, Rect};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

const DRAIN_SWEEP: Duration = Duration::from_millis(16);

pub type SessionId = u64;

/// One window render the pane worker produced for one session. Kept per
/// session so a frame pushed later (another pane's output) can show this
/// client its own window again without asking the worker to re-render it.
struct Window {
    /// Screen-space top row it was rendered from; a reply whose offset no
    /// longer matches the session's is stale and dropped.
    offset: usize,
    /// The emulator's geometry at render time. A pane that has since
    /// reflowed has moved every row in its scrollback, so the window is
    /// no longer a view of anything.
    cols: u16,
    rows: u16,
    text: String,
    lines: Vec<StyledLine>,
}

/// One attached client. Its read half is nonblocking so the daemon's loop
/// can sweep every session in one pass without blocking on any of them,
/// and it carries its own terminal size, since rects are computed per
/// session.
struct ClientSession {
    id: SessionId,
    reader: BufReader<UnixStream>,
    /// A clone of the same socket, used for frames and replies. Nonblocking
    /// too: `try_clone` shares the file description, and `write_msg` spins
    /// on WouldBlock rather than failing.
    writer: UnixStream,
    /// Partial line left over from a nonblocking read; bytes stay here
    /// until a newline completes them.
    buf: String,
    cols: u16,
    rows: u16,
    /// This client's viewport per pane: the screen-space top row it is
    /// showing. A pane absent from the map is following the live screen.
    /// The viewport lives here, not on the daemon or in the emulator, so
    /// one client reading history leaves every other client live.
    scroll: HashMap<PaneId, usize>,
    /// Latest window render per pane, from the worker, for the panes this
    /// session is showing above the live screen.
    windows: HashMap<PaneId, Window>,
    /// Whether this session has asked to attach. A connection exists from
    /// the accept, but it is not a client until it says so: the attach
    /// picker opens a connection to read the listing and drops it without
    /// ever attaching, and counting that as a client would have the picker
    /// report itself.
    attached: bool,
}

impl ClientSession {
    fn new(id: SessionId, stream: UnixStream, cols: u16, rows: u16) -> Result<Self> {
        let writer = stream.try_clone()?;
        stream.set_nonblocking(true)?;
        Ok(Self {
            id,
            reader: BufReader::new(stream),
            writer,
            buf: String::new(),
            cols,
            rows,
            scroll: HashMap::new(),
            windows: HashMap::new(),
            attached: false,
        })
    }

    /// This session's viewport top for one pane: the stored row, clamped
    /// to the pane's grid, or `total` when it is following the live
    /// screen. `total` is the live screen's first row, so a clamped
    /// result equal to `total` means live follow. Clamping covers a
    /// scrollback that shrank under a pinned row (a cleared pane).
    fn viewport_top(&self, pane: PaneId, total: usize) -> usize {
        self.scroll.get(&pane).copied().unwrap_or(total).min(total)
    }

    /// Pin this session's viewport for one pane, or return it to live
    /// follow when the target is the live screen.
    fn set_viewport(&mut self, pane: PaneId, top: usize, total: usize) {
        if top >= total {
            self.scroll.remove(&pane);
        } else {
            self.scroll.insert(pane, top);
        }
        self.windows.remove(&pane);
    }

    /// Drop a pane's viewport state; the pane is gone.
    fn forget_pane(&mut self, pane: PaneId) {
        self.scroll.remove(&pane);
        self.windows.remove(&pane);
    }

    /// Parse everything this client has queued. `None` means the client
    /// hung up, which is a detach, not a daemon error.
    fn read_msgs(&mut self) -> std::io::Result<Option<Vec<ClientMsg>>> {
        loop {
            match self.reader.read_line(&mut self.buf) {
                Ok(0) => return Ok(None),
                Ok(_) => {}
                // No more queued bytes; whatever is in `buf` is either a
                // complete line or a partial one to resume next pass.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        let mut msgs = Vec::new();
        while let Some(pos) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=pos).collect();
            if let Ok(msg) = serde_json::from_str::<ClientMsg>(line.trim()) {
                msgs.push(msg);
            }
        }
        Ok(Some(msgs))
    }
}

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
    /// The sizing client's terminal size, that being the client which most
    /// recently sent a message. A PTY has exactly one reflow width, so this
    /// is the size every pane wraps at; another client's rect only clips
    /// that content to fit (S4-1).
    cols: u16,
    rows: u16,
    /// This daemon's workspace id, named after the socket it serves so the
    /// id a client reads back identifies the daemon it just talked to.
    workspace: String,
    /// Clients attached right now, which is sessions that have sent
    /// `Attach` and not yet gone away. The session list holds the truth
    /// and only the serve loop can see it, so the loop keeps this current
    /// for the benefit of a listing request.
    clients: usize,
}

// The daemon core no longer touches emulators or PTYs: every pane state
// mutation happens on the pane's worker thread and reaches the core as
// a PaneOut snapshot on the shared channel.
impl Daemon {
    pub fn new(cols: u16, rows: u16, workspace: impl Into<String>) -> Self {
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
            workspace: workspace.into(),
            clients: 0,
        }
    }

    /// The workspace id for a daemon serving `listener`: the socket path's
    /// file stem. Read from the bound socket rather than the environment,
    /// because the environment is not what the daemon is actually serving,
    /// and both ends derive the id from the same path with no extra state.
    fn workspace_id(listener: &UnixListener) -> String {
        listener
            .local_addr()
            .ok()
            .and_then(|addr| addr.as_pathname().map(Path::to_path_buf))
            .as_deref()
            .and_then(Path::file_stem)
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "corral".into())
    }

    /// Serve clients forever. The daemon state (panes, scrollback, focus)
    /// lives here, not in any connection: a disconnect (Ctrl+a d) keeps
    /// every pane running, and the next connection attaches to the same
    /// session. Every attached client is served in the same pass, so a
    /// second client's connect never waits behind the first client's read.
    /// The caller owns the listener and socket cleanup.
    pub fn serve(listener: UnixListener) -> Result<()> {
        let workspace = Daemon::workspace_id(&listener);
        let mut daemon = Daemon::new(80, 24, workspace);
        // Nonblocking accept: the loop takes whatever the kernel has queued
        // and then moves on, instead of parking on one connection.
        listener.set_nonblocking(true)?;
        let mut sessions: Vec<ClientSession> = Vec::new();
        let mut next_session: SessionId = 1;
        loop {
            // A re-attaching client needs the current layout immediately:
            // without panes it creates the first shell, with panes it
            // attaches to the existing ones. Push the frame on connect so
            // the decision is frame-driven, not guessed.
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let mut session =
                            ClientSession::new(next_session, stream, daemon.cols, daemon.rows)?;
                        next_session += 1;
                        match daemon.push_frame(&mut session) {
                            Ok(()) => sessions.push(session),
                            // Went away between connect and first frame;
                            // nothing to attach to and nothing to clean up.
                            Err(e) if is_disconnect(&e) => {}
                            Err(e) => return Err(e),
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
            // One read pass over every session, then one drain of the pane
            // channel fanned out to all of them.
            let mut departed: Vec<SessionId> = Vec::new();
            for i in 0..sessions.len() {
                let msgs = match sessions[i].read_msgs() {
                    Ok(Some(msgs)) => msgs,
                    // The client hung up; that is a detach, not an error.
                    Ok(None) => {
                        departed.push(sessions[i].id);
                        continue;
                    }
                    // A broken socket is that one client's problem: drop it
                    // and keep serving the rest.
                    Err(_) => {
                        departed.push(sessions[i].id);
                        continue;
                    }
                };
                if msgs.is_empty() {
                    continue;
                }
                for msg in &msgs {
                    if let ClientMsg::Resize { cols, rows } = msg {
                        sessions[i].cols = *cols;
                        sessions[i].rows = *rows;
                    }
                }
                // Sending anything makes this the sizing client, so the
                // panes reflow to its size. A no-op when it already is.
                daemon.resize_to(sessions[i].cols, sessions[i].rows);
                let mut relayout = false;
                for msg in &msgs {
                    daemon.handle(msg, &mut sessions[i])?;
                    // Focus, Resize, and CreatePane change layout or focus
                    // without touching pane output; every attached client
                    // must see their effect. Key presses produce output that
                    // the drain sweep pushes, so they get no extra frame.
                    // A viewport message (Scroll, Search, PromptJump) is one
                    // client's window, so broadcasting a frame for it would
                    // hand every other client that client's offset with live
                    // text under it; the asking client's frame comes from its
                    // own window render instead.
                    relayout |= !matches!(
                        msg,
                        ClientMsg::Key { .. }
                            | ClientMsg::Scroll { .. }
                            | ClientMsg::Search { .. }
                            | ClientMsg::PromptJump { .. }
                    );
                }
                if relayout {
                    push_to_all(&daemon, &mut sessions, &mut departed)?;
                }
            }
            daemon.drain_and_push(&mut sessions, &mut departed)?;
            if !departed.is_empty() {
                sessions.retain(|s| !departed.contains(&s.id));
            }
            // A listing answers with this, and only the session list knows
            // it. Taken at the end of a pass, so a client that attached
            // during this one shows up in the next listing: the same
            // one-pass window a client that hangs up here falls into.
            daemon.clients = sessions.iter().filter(|s| s.attached).count();
        }
    }

    fn handle(&mut self, msg: &ClientMsg, session: &mut ClientSession) -> Result<()> {
        match msg {
            ClientMsg::Attach => session.attached = true,
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
                self.reflow_panes();
            }
            ClientMsg::Key { bytes } => {
                self.send_to_pane(self.focused, PaneCmd::Key(bytes.clone()));
            }
            ClientMsg::Resize { cols, rows } => {
                // The client's own terminal size lands in its session; the
                // size the panes wrap at follows the sizing client, which
                // the serve loop updates for any message, this one
                // included. Idempotent when the loop already applied it.
                self.resize_to(*cols, *rows);
            }
            ClientMsg::Focus { dir } => {
                if let Some(next) = self.root.focus_dir(self.focused, *dir) {
                    self.focused = next;
                }
            }
            ClientMsg::FocusNext => self.focus_next(),
            ClientMsg::Scroll { pane, target } => {
                let target = match target {
                    crate::protocol::ScrollTarget::Delta(d) => ScrollTarget::Delta(*d),
                    crate::protocol::ScrollTarget::Row(r) => ScrollTarget::Row(*r),
                    crate::protocol::ScrollTarget::Top => ScrollTarget::Top,
                    crate::protocol::ScrollTarget::Bottom => ScrollTarget::Bottom,
                };
                // The wheel scrolls the pane under the pointer, which the
                // client knows and the daemon does not, so the message names
                // it. A pane that has since gone away just drops the scroll.
                if self.panes.contains_key(pane) {
                    self.scroll_session(session, *pane, target)?;
                }
            }
            ClientMsg::Search {
                needle,
                from,
                reverse,
            } => {
                self.send_to_pane(
                    self.focused,
                    PaneCmd::Search {
                        session: session.id,
                        needle: needle.clone(),
                        from: *from,
                        reverse: *reverse,
                    },
                );
            }
            ClientMsg::ClearHistory => {
                self.send_to_pane(self.focused, PaneCmd::ClearHistory);
            }
            ClientMsg::PromptJump { up, cursor_row } => {
                let pane = self.focused;
                // The anchor is this session's window top, not the pane's:
                // two clients on one pane each jump from where they are
                // looking.
                let total = self.scrollback_total(pane);
                let top = session.viewport_top(pane, total);
                self.send_to_pane(
                    pane,
                    PaneCmd::PromptJump {
                        session: session.id,
                        up: *up,
                        cursor_row: *cursor_row,
                        top,
                    },
                );
            }
            ClientMsg::FocusPane { pane } => {
                if self.panes.contains_key(pane) {
                    self.focused = *pane;
                }
            }
            ClientMsg::SetSplitRatio { at, ratio } => {
                let area = Rect {
                    x: 0,
                    y: 0,
                    w: self.cols,
                    h: self.rows,
                };
                if self.root.set_ratio_near(area, *at, *ratio) {
                    self.reflow_panes();
                }
            }
            ClientMsg::ListWorkspaces => {
                // One workspace per daemon, and the panes are the ones the
                // daemon is holding. The reply names this daemon, so a
                // client can tell which one it reached.
                write_msg(
                    &mut session.writer,
                    &ServerMsg::WorkspaceList {
                        workspaces: vec![WorkspaceInfo {
                            id: self.workspace.clone(),
                            panes: self.panes.len(),
                            clients: self.clients,
                        }],
                    },
                )?;
            }
        }
        Ok(())
    }

    /// The pane's current scrollback total, from its latest snapshot. Zero
    /// for a pane that has produced no snapshot yet or has gone away.
    fn scrollback_total(&self, pane: PaneId) -> usize {
        self.snapshots.get(&pane).map_or(0, |s| s.total_scrollback)
    }

    /// Move one session's viewport for one pane and tell it how far it
    /// really moved. The clamp lives here rather than in the worker
    /// because the offset being clamped is the session's, so the daemon
    /// can answer `ScrollLanded` without a round trip. A scroll clamps at
    /// either end of the scrollback, so the answer can be smaller than the
    /// request; copy mode needs the real movement to keep its cursor off
    /// the edge the viewport pinned against (S3-3).
    fn scroll_session(
        &mut self,
        session: &mut ClientSession,
        pane: PaneId,
        target: ScrollTarget,
    ) -> Result<()> {
        let total = self.scrollback_total(pane);
        let from = session.viewport_top(pane, total);
        let to = match target {
            ScrollTarget::Delta(d) => (from as isize + d).clamp(0, total as isize) as usize,
            ScrollTarget::Row(row) => row.min(total),
            ScrollTarget::Top => 0,
            ScrollTarget::Bottom => total,
        };
        self.show_window(session, pane, to, total)?;
        write_msg(
            &mut session.writer,
            &ServerMsg::ScrollLanded {
                pane,
                moved: to as isize - from as isize,
            },
        )
    }

    /// Point one session's viewport at `top` (a screen-space row) and hand
    /// it the frame showing that window. At or past the live row the pane's
    /// snapshot already is that window, so no render is needed; above it,
    /// the pane worker renders the rows this client asked for, and the
    /// frame follows when that render arrives.
    fn show_window(
        &mut self,
        session: &mut ClientSession,
        pane: PaneId,
        top: usize,
        total: usize,
    ) -> Result<()> {
        session.set_viewport(pane, top, total);
        if session.scroll.contains_key(&pane) {
            let session_id = session.id;
            let rows = self.pane_rows(session, pane);
            self.send_to_pane(
                pane,
                PaneCmd::Window {
                    session: session_id,
                    offset: top.min(total),
                    rows,
                },
            );
            return Ok(());
        }
        self.push_frame(session)
    }

    /// How many rows one session can show of one pane: its own rect for
    /// that pane, which is what the client indexes a window by.
    fn pane_rows(&self, session: &ClientSession, pane: PaneId) -> u16 {
        self.rects(session.cols, session.rows)
            .into_iter()
            .find(|(id, _)| *id == pane)
            .map_or(session.rows, |(_, rect)| rect.h)
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

    /// Send one command to one pane. A closed channel means that pane's
    /// worker has stopped, which is how every pane ends: the worker queues
    /// its `Exited` and then drops its receiver, so the daemon either
    /// already has that message or will get it on a later sweep. The pane
    /// is gone, not the session, so the command is dropped. Treating it as
    /// fatal would let one pane's exit race take down every other pane and
    /// every attached client.
    fn send_to_pane(&self, id: PaneId, cmd: PaneCmd) {
        if let Some(pane) = self.panes.get(&id) {
            let _ = pane.send(cmd);
        }
    }

    /// Adopt a sizing client's terminal size and reflow the panes to it.
    fn resize_to(&mut self, cols: u16, rows: u16) {
        if self.cols == cols && self.rows == rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.reflow_panes();
    }

    /// Push the sizing client's rects to every pane's PTY and emulator, so
    /// pane output wraps at the width it is displayed at. Called whenever
    /// the tree changes shape or the sizing client changes size.
    fn reflow_panes(&self) {
        for (id, rect) in self.rects(self.cols, self.rows) {
            self.send_to_pane(id, PaneCmd::Resize(rect.w, rect.h));
        }
    }

    /// The layout one client sees, in that client's own terminal size.
    fn rects(&self, cols: u16, rows: u16) -> Vec<(PaneId, Rect)> {
        self.root.rects(Rect {
            x: 0,
            y: 0,
            w: cols,
            h: rows,
        })
    }

    fn drain_and_push(
        &mut self,
        sessions: &mut [ClientSession],
        departed: &mut Vec<SessionId>,
    ) -> Result<()> {
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
                Ok(PaneOut::Window {
                    session,
                    pane,
                    offset,
                    cols,
                    rows,
                    text,
                    lines,
                }) => {
                    // A window render belongs to the one client that asked
                    // for it, and only while it still describes where that
                    // client is looking: a superseded one (the client
                    // scrolled again, or detached) is dropped.
                    let Some(s) = sessions.iter_mut().find(|s| s.id == session) else {
                        continue;
                    };
                    if s.scroll.get(&pane) != Some(&offset) {
                        continue;
                    }
                    s.windows.insert(
                        pane,
                        Window {
                            offset,
                            cols,
                            rows,
                            text,
                            lines,
                        },
                    );
                    self.push_frame(s)?;
                }
                Ok(PaneOut::SearchResult {
                    session,
                    pane,
                    rows: hits,
                }) => {
                    // The first hit is the jump the search asked for; the
                    // client's n/N walk keeps its own resume offset from the
                    // frame. Showing it moves this session's window only, so
                    // a search in one client leaves the others live. The
                    // reply carries the window top this session ended up at,
                    // which is the hit when the jump pinned the window and
                    // the live row when the hit was already on screen.
                    let Some(s) = sessions.iter_mut().find(|s| s.id == session) else {
                        continue;
                    };
                    let total = self.scrollback_total(pane);
                    if let Some(&hit) = hits.first() {
                        self.show_window(s, pane, hit, total)?;
                    }
                    let top = s.viewport_top(pane, total);
                    write_msg(
                        &mut s.writer,
                        &ServerMsg::SearchResult {
                            pane,
                            rows: hits,
                            top,
                        },
                    )?;
                }
                Ok(PaneOut::PromptLanded {
                    session,
                    pane,
                    row,
                    col,
                    top,
                }) => {
                    // Only the asking client moves its copy cursor onto the
                    // command, and only its own window moves to show it.
                    let Some(s) = sessions.iter_mut().find(|s| s.id == session) else {
                        continue;
                    };
                    let total = self.scrollback_total(pane);
                    self.show_window(s, pane, top.unwrap_or(total), total)?;
                    write_msg(&mut s.writer, &ServerMsg::PromptLanded { pane, row, col })?;
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
        // Snapshots first, frame while the panes are still alive: every
        // client must see a pane's final output before the Exited
        // message collapses the tree.
        if changed || !exited.is_empty() {
            push_to_all(self, sessions, departed)?;
        }
        for id in &exited {
            self.panes.remove(id);
            self.snapshots.remove(id);
            // Pane ids are never reused, so a stale viewport entry could
            // not point at the wrong pane, but it would sit in every
            // session for the daemon's lifetime.
            for session in sessions.iter_mut() {
                session.forget_pane(*id);
            }
            let sibling = self.root.remove(*id);
            if let Some(sib) = sibling {
                self.focused = sib;
            }
            write_to_all(sessions, &ServerMsg::Exited { pane: *id }, departed)?;
        }
        if !exited.is_empty() {
            // The tree collapsed: surviving panes' rects grew to fill
            // the closed pane's space. Reflow each PTY and emulator to
            // the new size, same as CreatePane's split does, so the
            // remaining panes unwrap back to full width.
            self.reflow_panes();
            push_to_all(self, sessions, departed)?;
        }
        Ok(())
    }

    /// Whether a cached window still describes what one session should be
    /// shown for one pane. It stops describing anything once the pane
    /// reflows under it (every scrollback row moves, so the offset means
    /// something else) or once the scrollback shrinks past it (a cleared
    /// pane cannot hold that row).
    fn window_is_current(&self, pane: PaneId, window: &Window) -> bool {
        self.snapshots.get(&pane).is_some_and(|s| {
            s.rect.w == window.cols && s.rect.h == window.rows && s.total_scrollback > window.offset
        })
    }

    fn push_frame(&self, session: &mut ClientSession) -> Result<()> {
        let rects = self.rects(session.cols, session.rows);
        // Retire windows that no longer describe anything, and with them
        // the sessions pinned to them: showing stale rows is worse than
        // following the live screen again.
        let retired: Vec<PaneId> = session
            .windows
            .iter()
            .filter(|(id, w)| !self.window_is_current(**id, w))
            .map(|(id, _)| *id)
            .collect();
        for id in retired {
            session.forget_pane(id);
        }
        let mut panes = Vec::new();
        for (id, rect) in &rects {
            let Some(snapshot) = self.snapshots.get(id) else {
                continue;
            };
            let mut state = snapshot.clone();
            state.rect = *rect;
            match session.windows.get(id) {
                Some(window) => {
                    // This client is reading history: show its window
                    // instead of the live screen, and announce where it
                    // sits so copy mode and the position indicator
                    // agree with the rows on screen. The live cursor is
                    // not in this window, so it is not drawn.
                    state.text = window.text.clone();
                    state.lines = window.lines.clone();
                    state.cursor = None;
                    state.scroll = Some(ScrollPos {
                        offset: window.offset,
                        total: snapshot.total_scrollback,
                    });
                }
                None => state.scroll = None,
            }
            panes.push(state);
        }
        let msg = ServerMsg::Frame {
            panes,
            focused: self.focused,
        };
        write_msg(&mut session.writer, &msg)
    }
}

/// Write one frame to every session.
fn push_to_all(
    daemon: &Daemon,
    sessions: &mut [ClientSession],
    departed: &mut Vec<SessionId>,
) -> Result<()> {
    for_each_session(sessions, departed, |session| daemon.push_frame(session))
}

/// Write one message to every session.
fn write_to_all(
    sessions: &mut [ClientSession],
    msg: &ServerMsg,
    departed: &mut Vec<SessionId>,
) -> Result<()> {
    for_each_session(sessions, departed, |session| {
        write_msg(&mut session.writer, msg)
    })
}

/// One write attempt per session. A session that has gone away is collected
/// for removal; any other write error is fatal.
fn for_each_session(
    sessions: &mut [ClientSession],
    departed: &mut Vec<SessionId>,
    mut write: impl FnMut(&mut ClientSession) -> Result<()>,
) -> Result<()> {
    for session in sessions.iter_mut() {
        if let Err(e) = write(session) {
            if is_disconnect(&e) {
                departed.push(session.id);
            } else {
                return Err(e);
            }
        }
    }
    Ok(())
}

/// A frame write to a departed client reports BrokenPipe; that is a clean
/// disconnect, not a daemon error.
fn is_disconnect(e: &anyhow::Error) -> bool {
    e.root_cause()
        .downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
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

    /// A session over a paired socket, for tests that drive `handle`
    /// directly instead of through `serve`. The other half stays dropped:
    /// nothing here reads replies, and a write to a closed peer is only
    /// reachable from `push_frame`, which these tests do not call.
    fn session(id: SessionId) -> ClientSession {
        let (stream, _peer) = UnixStream::pair().unwrap();
        ClientSession::new(id, stream, 80, 24).unwrap()
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
    fn focus_pane_message_focuses_the_named_pane_directly() {
        let dir =
            std::env::temp_dir().join(format!("corral-test-focuspane-{}", std::process::id()));
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
        let ServerMsg::Frame { panes, focused } = frame else {
            unreachable!()
        };
        let other = panes
            .iter()
            .map(|p| p.id)
            .find(|&id| id != focused)
            .unwrap();
        send(reader.get_mut(), &ClientMsg::FocusPane { pane: other });
        let frame = wait_for_msg(&mut reader, |m| matches!(m, ServerMsg::Frame { .. }))
            .expect("frame after focus pane");
        let ServerMsg::Frame { focused, .. } = frame else {
            unreachable!()
        };
        assert_eq!(focused, other, "click focused the clicked pane directly");
        drop(reader);
        let _ = std::fs::remove_file(dir.join("s.sock"));
    }

    #[test]
    fn set_split_ratio_message_reflows_the_panes_to_the_new_ratio() {
        let dir = std::env::temp_dir().join(format!("corral-test-setratio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || Daemon::serve(listener).unwrap());

        let mut client = UnixStream::connect(&sock).unwrap();
        send(
            &mut client,
            &ClientMsg::Resize {
                cols: 100,
                rows: 40,
            },
        );
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
        let left = panes.iter().min_by_key(|p| p.rect.x).unwrap().id;

        send(
            reader.get_mut(),
            &ClientMsg::SetSplitRatio {
                at: left,
                ratio: 0.75,
            },
        );
        let frame = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.id == left && p.rect.w > 60),
            _ => false,
        })
        .expect("frame with the widened left pane");
        let ServerMsg::Frame { panes, .. } = frame else {
            unreachable!()
        };
        let left_rect = panes.iter().find(|p| p.id == left).unwrap().rect;
        assert!(
            left_rect.w > 60,
            "left pane should have grown toward 75% of 100 columns, got {left_rect:?}"
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
        // Two panes exit and the third stays alive, so the tree collapses
        // deterministically to the survivor. The survivor must be the only
        // pane in the frame and must hold focus. (Three short-lived panes
        // would not do: the daemon now handles a whole read batch before
        // emitting a frame, so all three exits land in one sweep and the
        // frame jumps straight from three panes to none.)
        let _sock = start_daemon("collapse");
        let mut client = UnixStream::connect(&_sock).unwrap();
        for out in ["one", "two", "three"] {
            let cmd = if out == "three" {
                format!("printf {out}; cat")
            } else {
                format!("printf {out}")
            };
            send(
                &mut client,
                &ClientMsg::CreatePane {
                    cmd: "sh".into(),
                    args: vec!["-c".into(), cmd],
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
    fn a_command_to_a_pane_whose_worker_stopped_is_not_fatal() {
        // A pane's worker stops the moment its command exits, taking the
        // pane's command channel with it. The daemon only learns that from
        // the worker's `Exited` message, which arrives on a later sweep.
        // A command aimed at the pane inside that window lands on a closed
        // channel: that is the pane being gone, not a daemon failure, and
        // it must not take the whole session down with it. No drain runs
        // here, so the window is held open deliberately.
        let mut daemon = Daemon::new(80, 24, "corrald-test");
        let mut client = session(1);
        for cmd in ["cat", "printf two"] {
            daemon
                .handle(
                    &ClientMsg::CreatePane {
                        cmd: "sh".into(),
                        args: vec!["-c".into(), cmd.into()],
                        cwd: "/tmp".into(),
                        dir: Dir::Horizontal,
                    },
                    &mut client,
                )
                .unwrap();
        }
        // Long enough for `printf two` and its worker to be gone.
        std::thread::sleep(Duration::from_millis(300));
        daemon.resize_to(100, 30);
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
                pane: 1,
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
                pane: 1,
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
    fn pane_pwd_from_osc_7_rides_the_frame_as_a_plain_path() {
        // The status bar names the focused pane's directory, so the pane
        // reports its own cwd with OSC 7 and the frame carries it decoded,
        // not as the raw `file://` URI the sequence contains.
        let sock = start_daemon("pwd");
        let mut client = UnixStream::connect(&sock).unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec![
                    "-c".into(),
                    "printf '\\033]7;file://localhost/tmp/here\\007'; sleep 30".into(),
                ],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
            },
        );
        let mut reader = BufReader::new(client);
        let reported = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.pwd == "/tmp/here"),
            _ => false,
        })
        .expect("frame with the OSC 7 directory within 5s");
        let ServerMsg::Frame { panes, .. } = reported else {
            unreachable!()
        };
        assert_eq!(panes[0].pwd, "/tmp/here");
        drop(reader);
    }

    #[test]
    fn scroll_landed_reports_the_clamped_move_at_the_scrollback_boundary() {
        let sock = start_daemon("scroll-landed");
        let mut client = UnixStream::connect(&sock).unwrap();
        // Short scrollback: 30 lines in a 24-row screen leaves well under
        // the half-page (12) the client asks for, so a Ctrl+u clamps.
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "seq 1 30; sleep 30".into()],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
            },
        );
        let mut reader = BufReader::new(client);
        wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes
                .iter()
                .any(|p| p.text.contains("30") && p.scroll.is_none()),
            _ => false,
        })
        .expect("bottom frame with line 30 and scroll None within 5s");

        // Pinned to the bottom, so the viewport top sits `total` rows up;
        // a half-page scroll runs out of history before it gets there.
        send(
            reader.get_mut(),
            &ClientMsg::Scroll {
                pane: 1,
                target: crate::protocol::ScrollTarget::Delta(-12),
            },
        );
        let landed = wait_for_msg(&mut reader, |m| matches!(m, ServerMsg::ScrollLanded { .. }))
            .expect("ScrollLanded reply within 5s");
        let ServerMsg::ScrollLanded { moved, .. } = landed else {
            unreachable!()
        };
        let scrolled = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.scroll.is_some()),
            _ => false,
        })
        .expect("scrolled frame with a scroll position within 5s");
        let ServerMsg::Frame { panes, .. } = scrolled else {
            unreachable!()
        };
        let total = panes[0].scroll.expect("scroll populated").total;
        assert!(
            total > 0 && total < 12,
            "pane needs a scrollback shorter than the half page, got {total}"
        );
        assert_eq!(
            moved,
            -(total as isize),
            "a clamped half-page scroll must report the rows it actually moved"
        );

        // Already at the top: the next half-page scroll moves nothing at
        // all, and the client learns that rather than assuming a full 12.
        send(
            reader.get_mut(),
            &ClientMsg::Scroll {
                pane: 1,
                target: crate::protocol::ScrollTarget::Delta(-12),
            },
        );
        let landed = wait_for_msg(&mut reader, |m| matches!(m, ServerMsg::ScrollLanded { .. }))
            .expect("second ScrollLanded reply within 5s");
        let ServerMsg::ScrollLanded { moved, .. } = landed else {
            unreachable!()
        };
        assert_eq!(moved, 0, "the viewport cannot move past the top");
        drop(reader);
    }

    #[test]
    fn a_named_pane_scrolls_while_the_focused_pane_stays_live() {
        // The wheel scrolls the pane under the pointer, which is not the
        // pane the daemon happens to focus. The message names the pane so
        // the daemon never has to guess, and the focused pane keeps
        // following the live screen meanwhile.
        let sock = start_daemon("wheel-pane");
        let mut client = UnixStream::connect(&sock).unwrap();
        for n in 0..2 {
            send(
                &mut client,
                &ClientMsg::CreatePane {
                    cmd: "sh".into(),
                    args: vec![
                        "-c".into(),
                        format!("seq 1 60; printf 'pane{n}-done'; sleep 30"),
                    ],
                    cwd: "/tmp".into(),
                    dir: Dir::Horizontal,
                },
            );
        }
        let mut reader = BufReader::new(client);
        let both = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => {
                panes.len() == 2 && panes.iter().all(|p| p.text.contains("-done"))
            }
            _ => false,
        })
        .expect("two panes with their output within 5s");
        let ServerMsg::Frame { panes, focused } = both else {
            unreachable!()
        };
        let other = panes
            .iter()
            .find(|p| p.id != focused)
            .expect("a pane that is not the focused one")
            .id;
        assert!(
            panes.iter().all(|p| p.total_scrollback > 1),
            "both panes need scrollback to scroll into: {:?}",
            panes
                .iter()
                .map(|p| (p.id, p.total_scrollback, p.rect.h))
                .collect::<Vec<_>>()
        );

        send(
            reader.get_mut(),
            &ClientMsg::Scroll {
                pane: other,
                target: crate::protocol::ScrollTarget::Top,
            },
        );
        let scrolled = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => {
                panes.iter().any(|p| p.id == other && p.scroll.is_some())
            }
            _ => false,
        })
        .expect("frame with the named pane scrolled within 5s");
        let ServerMsg::Frame { panes, .. } = scrolled else {
            unreachable!()
        };
        assert!(
            panes.iter().any(|p| p.id == focused && p.scroll.is_none()),
            "the focused pane must stay live while another pane is scrolled, got {:?}",
            panes.iter().map(|p| (p.id, p.scroll)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn list_workspaces_reports_the_live_pane_and_client_counts() {
        // `corral attach` shows what is already running: how many panes
        // the daemon holds and how many clients are on it right now. A
        // stale or invented count sends the user to a workspace that is
        // not there.
        let sock = start_daemon("workspace-list");
        let mut a = UnixStream::connect(&sock).unwrap();
        let mut ra = BufReader::new(a.try_clone().unwrap());
        send(&mut a, &ClientMsg::Attach);
        for cmd in ["sleep 30", "sleep 30"] {
            send(
                &mut a,
                &ClientMsg::CreatePane {
                    cmd: "sh".into(),
                    args: vec!["-c".into(), cmd.into()],
                    cwd: "/tmp".into(),
                    dir: Dir::Horizontal,
                },
            );
        }
        wait_for_msg(&mut ra, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.len() == 2,
            _ => false,
        })
        .expect("a frame with both panes within 5s");

        send(&mut a, &ClientMsg::ListWorkspaces);
        let listed = wait_for_msg(&mut ra, |m| matches!(m, ServerMsg::WorkspaceList { .. }))
            .expect("the workspace list within 5s");
        let ServerMsg::WorkspaceList { workspaces } = listed else {
            unreachable!()
        };
        assert_eq!(workspaces.len(), 1, "one workspace per daemon");
        assert_eq!(workspaces[0].panes, 2, "both live panes counted");
        assert_eq!(workspaces[0].clients, 1, "only A is attached so far");
        let id = workspaces[0].id.clone();
        assert!(
            !id.is_empty(),
            "the picker shows this id and the user types it back"
        );

        // The picker reads the listing over a connection of its own and
        // drops it without attaching, so that connection must not count
        // itself: "1 client" is the count a user reads while deciding.
        let mut picker = UnixStream::connect(&sock).unwrap();
        let mut rp = BufReader::new(picker.try_clone().unwrap());
        send(&mut picker, &ClientMsg::ListWorkspaces);
        let listed = wait_for_msg(&mut rp, |m| matches!(m, ServerMsg::WorkspaceList { .. }))
            .expect("the picker's listing within 5s");
        let ServerMsg::WorkspaceList { workspaces } = listed else {
            unreachable!()
        };
        assert_eq!(
            workspaces[0].clients, 1,
            "a connection that never attached is not a client"
        );
        drop(rp);
        drop(picker);

        // A second client raises the count, and the id does not move: it
        // is what `corral attach <id>` is given, so it has to outlive any
        // one listing.
        let mut b = UnixStream::connect(&sock).unwrap();
        let mut rb = BufReader::new(b.try_clone().unwrap());
        send(&mut b, &ClientMsg::Attach);
        wait_for_msg(&mut rb, |m| matches!(m, ServerMsg::Frame { .. }))
            .expect("B's attach frame within 5s");
        send(&mut a, &ClientMsg::ListWorkspaces);
        let listed = wait_for_msg(&mut ra, |m| matches!(m, ServerMsg::WorkspaceList { .. }))
            .expect("the second workspace list within 5s");
        let ServerMsg::WorkspaceList { workspaces } = listed else {
            unreachable!()
        };
        assert_eq!(workspaces[0].clients, 2, "B is attached too");
        assert_eq!(
            workspaces[0].id, id,
            "the id is stable for the daemon's lifetime"
        );
        drop(ra);
        drop(rb);
    }

    /// Read and discard every message already queued, so the next message
    /// read is one the event under test caused. `quiet` is how long the
    /// stream must stay silent before it counts as drained.
    fn drain_queued(reader: &mut BufReader<UnixStream>, quiet: Duration) {
        reader.get_ref().set_read_timeout(Some(quiet)).unwrap();
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    }

    #[test]
    fn one_clients_scroll_leaves_the_other_client_live() {
        // Scrolling is a per-client view. One client scrolling into
        // history must leave the other following the live screen: a
        // viewport the whole daemon shares would pin both.
        let sock = start_daemon("per-client-viewport");
        let mut a = UnixStream::connect(&sock).unwrap();
        send(
            &mut a,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec!["-c".into(), "seq 1 60; sleep 30".into()],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
            },
        );
        let mut ra = BufReader::new(a);
        // `seq 1 60` leaves 61 rows of content in a 24-row pane: the
        // trailing newline adds a blank row, so 37 rows sit in history
        // and the live screen starts at line 38. A half-page scroll
        // moves the window 12 rows up, onto line 26.
        wait_for_msg(&mut ra, |m| match m {
            ServerMsg::Frame { panes, .. } => panes
                .iter()
                .any(|p| p.text.lines().next() == Some("38") && p.scroll.is_none()),
            _ => false,
        })
        .expect("A's live frame within 5s");

        let b = UnixStream::connect(&sock).unwrap();
        let mut rb = BufReader::new(b);
        wait_for_msg(&mut rb, |m| matches!(m, ServerMsg::Frame { .. }))
            .expect("B's attach frame within 5s");

        send(
            ra.get_mut(),
            &ClientMsg::Scroll {
                pane: 1,
                target: crate::protocol::ScrollTarget::Delta(-12),
            },
        );
        let scrolled = wait_for_msg(&mut ra, |m| match m {
            ServerMsg::Frame { panes, .. } => panes
                .iter()
                .any(|p| p.scroll.is_some() && p.text.lines().next() == Some("26")),
            _ => false,
        })
        .expect("A's scrolled frame within 5s");
        let ServerMsg::Frame { panes, .. } = scrolled else {
            unreachable!()
        };
        assert_eq!(
            panes[0].text.lines().next(),
            Some("26"),
            "A shows history after scrolling: {}",
            panes[0].text
        );

        // B asks for a frame after A's scroll. Its answer must be
        // untouched and still announced as live follow, both in content
        // and in the scroll position the client drives copy mode from.
        // An `Attach` is the one client message guaranteed to produce a
        // frame on demand; anything already queued for B is discarded
        // first, so the frame read below is one this asks for and not a
        // leftover from before the scroll.
        drain_queued(&mut rb, Duration::from_millis(150));
        send(rb.get_mut(), &ClientMsg::Attach);
        let b_frame = wait_for_msg(&mut rb, |m| matches!(m, ServerMsg::Frame { .. }))
            .expect("B's frame after A's scroll within 5s");
        let ServerMsg::Frame { panes, .. } = b_frame else {
            unreachable!()
        };
        assert!(
            panes[0].scroll.is_none(),
            "B must stay live-following, got {:?}",
            panes[0].scroll
        );
        assert_eq!(
            panes[0].text.lines().next(),
            Some("38"),
            "B keeps the live screen: {}",
            panes[0].text
        );
        drop(ra);
        drop(rb);
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
    fn clear_history_empties_scrollback_and_clears_the_screen_above_the_cursor() {
        let sock = start_daemon("clear");
        let mut client = UnixStream::connect(&sock).unwrap();
        send(
            &mut client,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                // The trailing `printf` (no newline) leaves unsubmitted
                // text on the cursor's row, so there is something above
                // the cursor to erase and something on the cursor's row
                // that must survive.
                args: vec!["-c".into(), "seq 1 60; printf prompt-cmd; sleep 30".into()],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
            },
        );
        let mut reader = BufReader::new(client);
        let bottom = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes
                .iter()
                .any(|p| p.text.contains("60") && p.text.contains("prompt-cmd")),
            _ => false,
        })
        .expect("bottom frame showing line 60 and the prompt row within 5s");
        let ServerMsg::Frame { panes, .. } = bottom else {
            unreachable!()
        };
        assert!(
            panes[0].text.contains("59"),
            "row above the cursor is visible pre-clear, got {:?}",
            panes[0].text
        );

        // Scrolled up first so the viewport is not pinned to the bottom.
        send(
            reader.get_mut(),
            &ClientMsg::Scroll {
                pane: 1,
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
        // any further Top scroll lands right back on the live screen. What
        // it shows is the active screen cleared of everything but the
        // cursor's row, which moves to the top of the screen.
        send(
            reader.get_mut(),
            &ClientMsg::Scroll {
                pane: 1,
                target: crate::protocol::ScrollTarget::Top,
            },
        );
        let settled = wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes
                .iter()
                .any(|p| p.text.contains("prompt-cmd") && !p.text.contains("60")),
            _ => false,
        })
        .expect("frame showing the cleared screen within 5s");
        let ServerMsg::Frame { panes, .. } = settled else {
            unreachable!()
        };
        assert!(
            !panes[0].text.contains("59"),
            "the visible screen above the cursor is erased, got {:?}",
            panes[0].text
        );
        assert_eq!(
            panes[0].text.lines().next(),
            Some("prompt-cmd"),
            "the prompt row rides up to the top of the screen, got {:?}",
            panes[0].text
        );
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

        // Jumping up past the first command stays on it rather than
        // overshooting into the blank space above its marker.
        send(
            reader.get_mut(),
            &ClientMsg::PromptJump {
                up: true,
                cursor_row: Some(cursor_row),
            },
        );
        let landed = wait_for_msg(&mut reader, |m| matches!(m, ServerMsg::PromptLanded { .. }))
            .expect("PromptLanded reply within 5s");
        let ServerMsg::PromptLanded { row: r, .. } = landed else {
            unreachable!()
        };
        assert_eq!(
            r, cursor_row,
            "up-jump past the first command must stay put"
        );
        wait_for_msg(&mut reader, |m| match m {
            ServerMsg::Frame { panes, .. } => panes
                .iter()
                .any(|p| p.scroll.is_some() && p.text.contains("prompt one")),
            _ => false,
        })
        .expect("frame still showing prompt one after the boundary up-jump within 5s");

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

        drop(reader);
    }

    /// S4-1: two clients attached at once both see a pane's later output.
    /// The second client connects while the first is still attached and
    /// still reading, which is what `serve`'s one-connection-at-a-time loop
    /// cannot do: the kernel accepts the second connection into the backlog
    /// and the daemon then starves it until the first client hangs up.
    #[test]
    fn both_clients_see_a_panes_output() {
        let sock = start_daemon("multi-fanout");
        let mut first = UnixStream::connect(&sock).unwrap();
        send(
            &mut first,
            &ClientMsg::CreatePane {
                cmd: "sh".into(),
                args: vec![
                    "-c".into(),
                    "printf one; sleep 2; printf two; sleep 20".into(),
                ],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
            },
        );
        let mut first = BufReader::new(first);
        wait_for_msg(&mut first, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.text.contains("one")),
            _ => false,
        })
        .expect("first client sees the pane come up within 5s");

        let mut second = BufReader::new(UnixStream::connect(&sock).unwrap());
        wait_for_msg(&mut second, |m| matches!(m, ServerMsg::Frame { .. }))
            .expect("second client gets an attach frame within 5s");

        // Output produced after both attached has to reach both. `two` is
        // printed two seconds in, so it is only visible to whoever is
        // attached when the worker pushes it.
        wait_for_msg(&mut first, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.text.contains("two")),
            _ => false,
        })
        .expect("first client sees the pane's later output within 5s");
        wait_for_msg(&mut second, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.text.contains("two")),
            _ => false,
        })
        .expect("second client sees the pane's later output within 5s");
    }

    /// S4-1: a connect is not serialized behind an attached client. Nothing
    /// produces output here, so the only thing that can send this frame is
    /// the accept loop running concurrently with the first client's read.
    #[test]
    fn a_second_client_is_served_while_the_first_is_attached() {
        let sock = start_daemon("multi-accept");
        let mut idle = UnixStream::connect(&sock).unwrap();
        send(&mut idle, &ClientMsg::Attach);
        let mut idle = BufReader::new(idle);
        wait_for_msg(&mut idle, |m| matches!(m, ServerMsg::Frame { .. }))
            .expect("first client attaches within 5s");

        let started = Instant::now();
        let mut second = BufReader::new(UnixStream::connect(&sock).unwrap());
        wait_for_msg(&mut second, |m| matches!(m, ServerMsg::Frame { .. }))
            .expect("second client gets an attach frame while the first is attached");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "second client waited {:?} for its attach frame; it queued behind the first",
            started.elapsed()
        );
    }

    /// S4-1: a detach is a detach, not a shutdown. The panes outlive the
    /// client that created them: a later attach sees the same pane id and
    /// the PTY behind it still answers.
    #[test]
    fn detach_keeps_the_pane_and_its_pty_alive() {
        let sock = start_daemon("multi-detach");
        let mut first = UnixStream::connect(&sock).unwrap();
        send(
            &mut first,
            &ClientMsg::CreatePane {
                cmd: "cat".into(),
                args: vec![],
                cwd: "/tmp".into(),
                dir: Dir::Horizontal,
            },
        );
        let mut first = BufReader::new(first);
        let frame = wait_for_msg(&mut first, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.len() == 1,
            _ => false,
        })
        .expect("first client sees the pane within 5s");
        let ServerMsg::Frame { panes, .. } = frame else {
            unreachable!()
        };
        let pane = panes[0].id;
        drop(first);

        // Same pane id after the detach: the session survived, rather than
        // being torn down and rebuilt.
        let mut second = BufReader::new(UnixStream::connect(&sock).unwrap());
        wait_for_msg(&mut second, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.id == pane),
            _ => false,
        })
        .expect("re-attach sees the same pane id within 5s");

        // Alive, not just a cached snapshot: cat echoes what it reads, so
        // output arriving now proves the process is still on its PTY.
        send(
            second.get_mut(),
            &ClientMsg::Key {
                bytes: b"still here\n".to_vec(),
            },
        );
        wait_for_msg(&mut second, |m| match m {
            ServerMsg::Frame { panes, .. } => panes.iter().any(|p| p.text.contains("still here")),
            _ => false,
        })
        .expect("the pane's PTY answers after a re-attach within 5s");
    }
}
