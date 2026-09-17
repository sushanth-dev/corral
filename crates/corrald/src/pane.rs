//! Per-pane worker threads. Each worker owns one pane's `Emulator` and
//! `PtyHandle` (libghostty-vt types are !Send, so this is the one thread
//! where the emulator lives and is used); the daemon core keeps only the
//! routing tree and the latest snapshot per pane.

use crate::pty::{PtyEvent, PtyHandle};
use crate::server::SessionId;
use corral_core::emulation::{Emulator, StyledLine};
use corral_core::tree::{PaneId, Rect};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

pub enum PaneCmd {
    /// Bytes to feed the emulator (query replies route through here).
    /// Callers arrive with the scrollback tasks (S3-2 onward); the
    /// worker handles it now so the protocol surface is complete.
    #[allow(dead_code)]
    Feed(Vec<u8>),
    /// Raw key bytes for the pane's PTY writer.
    Key(Vec<u8>),
    /// Resize the PTY and the emulator to the pane's new rect size.
    Resize(u16, u16),
    /// Render one client session's window of screen-space rows, without
    /// moving the emulator's own viewport (S4-1b). `offset` is a
    /// screen-space row, so it may sit above the live screen; `rows` is
    /// how many rows that client can show. The session is echoed in the
    /// reply so the daemon can route the render to the client that asked
    /// instead of broadcasting it.
    Window {
        session: SessionId,
        offset: usize,
        rows: u16,
    },
    /// Find rows containing a needle (S3-5). The reply rides PaneOut as
    /// a SearchResult the daemon core answers the asking session with.
    Search {
        session: SessionId,
        needle: String,
        from: Option<usize>,
        reverse: bool,
    },
    /// Erase the pane's scrollback (S3-6).
    ClearHistory,
    /// Resolve the previous (up) or next (down) OSC133 prompt row
    /// (S3-8) against one session's viewport. `cursor_row` anchors the
    /// walk at the copy cursor inside that session's viewport; `None`
    /// anchors at its bottom. `top` is that viewport's top row, so a
    /// jump continues from where this client is looking.
    PromptJump {
        session: SessionId,
        up: bool,
        cursor_row: Option<usize>,
        top: usize,
    },
    /// Rebuild and resend the snapshot even if nothing changed.
    #[allow(dead_code)]
    Render,
}

pub enum PaneOut {
    Snapshot {
        pane: PaneId,
        state: crate::protocol::PaneState,
    },
    /// Reply to PaneCmd::Window: the requested rows as plain text plus
    /// styled runs, with the emulator geometry they were rendered at so
    /// the daemon can tell a window the pane has since reflowed out from
    /// under it. `cols` and `rows` are the emulator's own size, not the
    /// window height that was asked for.
    Window {
        session: SessionId,
        pane: PaneId,
        offset: usize,
        cols: u16,
        rows: u16,
        text: String,
        lines: Vec<StyledLine>,
    },
    /// Reply to PaneCmd::Search: rows are screen-space row indexes. The
    /// viewport did not move, so which window shows the hit is the
    /// asking session's offset, applied by the daemon.
    SearchResult {
        session: SessionId,
        pane: PaneId,
        rows: Vec<usize>,
    },
    /// Reply to PaneCmd::PromptJump: where the client's copy cursor
    /// goes, as a row inside the window the daemon is about to show it,
    /// plus the window top to show, `None` meaning follow the live
    /// screen. The "up" walk with no prompts in the pane at all parks at
    /// the very top, which the row alone cannot express.
    PromptLanded {
        session: SessionId,
        pane: PaneId,
        row: usize,
        col: usize,
        top: Option<usize>,
    },
    Exited {
        pane: PaneId,
    },
}

pub struct PaneWorker;

impl PaneWorker {
    /// Spawn the worker thread owning `pty` and its emulator. Returns
    /// the command sender; the worker sends PaneOut on `out` and ends
    /// when the pane's shell exits.
    pub fn spawn(
        id: PaneId,
        pty: PtyHandle,
        cols: u16,
        rows: u16,
        out: Sender<PaneOut>,
    ) -> Sender<PaneCmd> {
        let (tx, rx): (Sender<PaneCmd>, Receiver<PaneCmd>) = channel();
        std::thread::Builder::new()
            .name(format!("pane-{id}"))
            .spawn(move || run_worker(id, pty, cols, rows, out, rx))
            .expect("spawn pane worker thread");
        tx
    }
}

fn run_worker(
    id: PaneId,
    mut pty: PtyHandle,
    mut cols: u16,
    mut rows: u16,
    out: Sender<PaneOut>,
    rx: Receiver<PaneCmd>,
) {
    let mut emu = match Emulator::new(cols, rows) {
        Ok(e) => e,
        Err(_) => {
            let _ = out.send(PaneOut::Exited { pane: id });
            return;
        }
    };
    let mut exited = false;
    // Initial snapshot before the loop: the daemon must be able to
    // render a frame containing this pane before any output arrives
    // (a quiet pane, `sleep 30`, still tiles into the layout).
    push_snapshot(id, &mut emu, cols, rows, &out);
    loop {
        if exited {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(2)) {
            Ok(cmd) => match cmd {
                PaneCmd::Feed(bytes) => {
                    emu.feed(&bytes);
                    push_snapshot(id, &mut emu, cols, rows, &out);
                }
                PaneCmd::Key(bytes) => {
                    let _ = pty.write_all(&bytes);
                }
                PaneCmd::Resize(w, h) => {
                    let _ = pty.resize(w, h);
                    if emu.resize(w, h).is_ok() {
                        // The dims every later snapshot is stamped with
                        // have to track the emulator's: the daemon decides
                        // whether a client's window still describes its
                        // pane by comparing the two, so a stale rect makes
                        // every scroll into a pane that has been resized
                        // look outdated and get thrown away.
                        cols = w;
                        rows = h;
                        push_snapshot(id, &mut emu, cols, rows, &out);
                    }
                }
                PaneCmd::Window {
                    session,
                    offset,
                    rows: height,
                } => {
                    // Read the rows straight out of the grid. The
                    // emulator's own viewport stays where it is, so one
                    // session's scroll cannot move what another session
                    // or the live screen shows.
                    let (text, lines) = emu.window_at(offset, height).unwrap_or_default();
                    let _ = out.send(PaneOut::Window {
                        session,
                        pane: id,
                        offset,
                        cols: emu.cols().unwrap_or(cols),
                        rows: emu.rows().unwrap_or(rows),
                        text,
                        lines,
                    });
                }
                PaneCmd::Search {
                    session,
                    needle,
                    from,
                    reverse,
                } => {
                    // Report the hits and leave the viewport alone: the
                    // daemon shows the first hit in the asking session's
                    // window, so a search in one client does not move
                    // another client's viewport.
                    let hits = emu.search(&needle, from, reverse).unwrap_or_default();
                    let _ = out.send(PaneOut::SearchResult {
                        session,
                        pane: id,
                        rows: hits,
                    });
                }
                PaneCmd::ClearHistory => {
                    // Erase scrollback only (3J); the live screen and
                    // whatever the user has typed at the prompt stay
                    // exactly as they are.
                    emu.clear_history();
                    push_snapshot(id, &mut emu, cols, rows, &out);
                }
                PaneCmd::PromptJump {
                    session,
                    up,
                    cursor_row,
                    top,
                } => {
                    // Command input positions, not raw OSC 133;A marker
                    // rows: a shell theme (Tide) draws a decorative box
                    // around the marker row, so jumping to the marker
                    // lands on the box instead of the typed command.
                    // prompt_input_positions resolves the exact
                    // (row, col) of each command via OSC 133;B.
                    let prompts = emu.prompt_input_positions().unwrap_or_default();
                    // Anchor at the copy cursor, not this session's
                    // viewport top: a jump to a prompt inside the
                    // visible screen clamps to the bottom, so a
                    // viewport-top anchor re-finds the same prompt
                    // forever. Screen space: session top + cursor row.
                    let anchor = top + cursor_row.unwrap_or(rows as usize - 1);
                    let total = emu.scrollback_rows().unwrap_or(0);
                    // `None` for the window top means follow the live
                    // screen; `Some(row)` pins this session there. A
                    // target inside the visible screen clamps to the
                    // live bottom, which is what `min(total)` is.
                    let (target, top) = match resolve_prompt_jump(&prompts, anchor, up) {
                        Some(prompt) => (Some(prompt), Some(prompt.0.min(total))),
                        None if up => match prompts.first().copied() {
                            // Already on the first command: land on it
                            // rather than overshooting into blank space
                            // above.
                            Some(prompt) => (Some(prompt), Some(prompt.0.min(total))),
                            // No prompts in the pane at all: the very
                            // top is as far as "up" goes.
                            None => (None, Some(0)),
                        },
                        // No prompt below: back to the live screen.
                        None => (None, None),
                    };
                    // The landing row is relative to the window the
                    // daemon will show, so the client can put its copy
                    // cursor on it; with no prompt to land on, the
                    // cursor's row stays put and the column resets.
                    let (row, col) = match target {
                        Some((prompt_row, prompt_col)) => (
                            prompt_row.saturating_sub(top.unwrap_or(0)),
                            prompt_col as usize,
                        ),
                        None => (cursor_row.unwrap_or(rows as usize - 1), 0),
                    };
                    let _ = out.send(PaneOut::PromptLanded {
                        session,
                        pane: id,
                        row,
                        col,
                        top,
                    });
                }
                PaneCmd::Render => {
                    push_snapshot(id, &mut emu, cols, rows, &out);
                }
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            // All command senders dropped: the daemon is shutting down.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // Drain whatever the PTY reader produced, then answer emulator
        // queries back into the PTY.
        let mut got_output = false;
        while let Ok(event) = pty.rx.try_recv() {
            match event {
                PtyEvent::Output(bytes) => {
                    emu.feed(&bytes);
                    for reply in emu.take_pty_writes() {
                        let _ = pty.write_all(&reply);
                    }
                    got_output = true;
                }
                PtyEvent::Exited => exited = true,
            }
        }
        if got_output {
            push_snapshot(id, &mut emu, cols, rows, &out);
        }
    }
    let _ = out.send(PaneOut::Exited { pane: id });
}

/// Picks the prompt to jump to from `anchor` (screen-space row) among
/// `prompts` (resolved command input positions, in row order).
fn resolve_prompt_jump(prompts: &[(usize, u16)], anchor: usize, up: bool) -> Option<(usize, u16)> {
    if up {
        prompts.iter().rev().find(|&&(r, _)| r < anchor).copied()
    } else {
        prompts.iter().find(|&&(r, _)| r > anchor).copied()
    }
}

fn push_snapshot(id: PaneId, emu: &mut Emulator, cols: u16, rows: u16, out: &Sender<PaneOut>) {
    // The snapshot is always the live screen: the viewport belongs to
    // each client session, so the pane's own viewport never leaves the
    // bottom. The daemon fills `scroll` per session when it builds a
    // frame, from each session's offset.
    let state = crate::protocol::PaneState {
        id,
        rect: Rect {
            x: 0,
            y: 0,
            w: cols,
            h: rows,
        },
        text: emu.screen_text().unwrap_or_default(),
        cursor: emu.cursor().unwrap_or(None),
        app_cursor: emu.app_cursor().unwrap_or(false),
        scroll: None,
        total_scrollback: emu.scrollback_rows().unwrap_or(0),
        lines: emu.screen_lines().unwrap_or_default(),
        pwd: emu.pwd().unwrap_or_default(),
    };
    let _ = out.send(PaneOut::Snapshot { pane: id, state });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    #[test]
    fn two_workers_deliver_snapshots_with_correct_pane_ids() {
        let (out, out_rx) = channel();
        let a = PtyHandle::spawn("sh", &["-c", "printf hello"], Path::new("/tmp"), 80, 24).unwrap();
        let b = PtyHandle::spawn("sh", &["-c", "printf world"], Path::new("/tmp"), 80, 24).unwrap();
        let ca = PaneWorker::spawn(7, a, 80, 24, out.clone());
        let cb = PaneWorker::spawn(9, b, 80, 24, out);

        let mut got_hello = false;
        let mut got_world = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !(got_hello && got_world) && std::time::Instant::now() < deadline {
            match out_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(PaneOut::SearchResult { .. }) => {}
                Ok(PaneOut::PromptLanded { .. }) => {}
                Ok(PaneOut::Window { .. }) => {}
                Ok(PaneOut::Snapshot { pane, state }) => {
                    assert!(pane == 7 || pane == 9, "unknown pane id {pane}");
                    if state.text.contains("hello") {
                        assert_eq!(pane, 7, "hello landed in the wrong pane");
                        got_hello = true;
                    }
                    if state.text.contains("world") {
                        assert_eq!(pane, 9, "world landed in the wrong pane");
                        got_world = true;
                    }
                }
                Ok(PaneOut::Exited { .. }) => {}
                Err(_) => {}
            }
        }
        assert!(got_hello && got_world, "both snapshots must arrive");
        drop(ca);
        drop(cb);
    }

    // Tide fixture shape: 3 prompts at rows 2, 10, 18 in a 24-row screen.
    const TIDE_PROMPTS: [(usize, u16); 3] = [(2, 2), (10, 2), (18, 2)];

    #[test]
    fn prompt_jump_up_finds_the_nearest_prompt_above_the_anchor() {
        assert_eq!(resolve_prompt_jump(&TIDE_PROMPTS, 19, true), Some((18, 2)));
    }

    #[test]
    fn prompt_jump_up_returns_none_at_the_first_command() {
        // Anchored on the first command itself: there is no prompt
        // above it, so the caller must fall back to it (not scroll to
        // the absolute top, which can overshoot into blank space).
        assert_eq!(resolve_prompt_jump(&TIDE_PROMPTS, 2, true), None);
    }

    #[test]
    fn prompt_jump_down_finds_the_nearest_prompt_below_the_anchor() {
        assert_eq!(resolve_prompt_jump(&TIDE_PROMPTS, 10, false), Some((18, 2)));
    }
}
