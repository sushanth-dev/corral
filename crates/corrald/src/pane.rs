//! Per-pane worker threads. Each worker owns one pane's `Emulator` and
//! `PtyHandle` (libghostty-vt types are !Send, so this is the one thread
//! where the emulator lives and is used); the daemon core keeps only the
//! routing tree and the latest snapshot per pane.

use crate::pty::{PtyEvent, PtyHandle};
use corral_core::emulation::{Emulator, ScrollTarget};
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
    /// Move the pane's viewport inside scrollback (S3-2).
    Scroll(ScrollTarget),
    /// Find rows containing a needle (S3-5). The reply rides PaneOut as
    /// a SearchResult the daemon core forwards to the client.
    Search {
        needle: String,
        from: Option<usize>,
        reverse: bool,
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
    /// Reply to PaneCmd::Search; rows are screen-space row indexes.
    SearchResult {
        pane: PaneId,
        rows: Vec<usize>,
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
    cols: u16,
    rows: u16,
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
                        push_snapshot(id, &mut emu, w, h, &out);
                    }
                }
                PaneCmd::Scroll(target) => {
                    emu.scroll(target);
                    push_snapshot(id, &mut emu, cols, rows, &out);
                }
                PaneCmd::Search {
                    needle,
                    from,
                    reverse,
                } => {
                    let hits = emu.search(&needle, from, reverse).unwrap_or_default();
                    // Jump to the first hit so the match is on screen;
                    // the client's n/N walk keeps its own resume offset.
                    if let Some(&row) = hits.first() {
                        emu.scroll(ScrollTarget::Row(row));
                        push_snapshot(id, &mut emu, cols, rows, &out);
                    }
                    let _ = out.send(PaneOut::SearchResult {
                        pane: id,
                        rows: hits,
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

fn push_snapshot(id: PaneId, emu: &mut Emulator, cols: u16, rows: u16, out: &Sender<PaneOut>) {
    let scroll = emu
        .scroll_position()
        .unwrap_or(None)
        .map(|p| crate::protocol::ScrollPos {
            offset: p.offset,
            total: p.total,
        });
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
        scroll,
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
}
