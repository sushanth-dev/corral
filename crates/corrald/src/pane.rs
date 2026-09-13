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
    /// Erase the pane's scrollback (S3-6).
    ClearHistory,
    /// Produce the pane's full scrollback text (S3-7); the reply rides
    /// PaneOut as a ScrollbackDump the daemon core forwards to the client.
    DumpScrollback,
    /// Jump the viewport to the previous (up) or next (down) OSC133
    /// prompt row (S3-8). `cursor_row` anchors the walk at the copy
    /// cursor inside the viewport; `None` anchors at the viewport
    /// bottom.
    PromptJump { up: bool, cursor_row: Option<usize> },
    /// Erase the pane's scrollback and feed `text` back through the
    /// emulator (S3-7 write-back): the terminal view shows the editor's
    /// result.
    LoadScrollback { text: String },
    /// Extract the command block ending at or before `anchor` (S3-8);
    /// the reply rides PaneOut as a ScrollbackDump.
    YankCommand { anchor: Option<usize> },
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
    /// Reply to PaneCmd::DumpScrollback (S3-7).
    ScrollbackDump {
        pane: PaneId,
        text: String,
    },
    /// Reply to PaneCmd::PromptJump: the prompt landed at this row
    /// inside the viewport.
    PromptLanded {
        pane: PaneId,
        row: usize,
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
                PaneCmd::ClearHistory => {
                    emu.clear_history();
                    push_snapshot(id, &mut emu, cols, rows, &out);
                }
                PaneCmd::DumpScrollback => {
                    let text = emu.dump_scrollback().unwrap_or_default();
                    let _ = out.send(PaneOut::ScrollbackDump { pane: id, text });
                }
                PaneCmd::PromptJump { up, cursor_row } => {
                    let prompts = emu.prompt_rows().unwrap_or_default();
                    // Anchor at the copy cursor, not the viewport top:
                    // a Row scroll to a prompt inside the visible
                    // screen clamps to the bottom, so a viewport-top
                    // anchor re-finds the same prompt forever.
                    // Screen-space anchor = viewport top + cursor row;
                    // when pinned to the bottom the viewport top is
                    // total minus the screen height.
                    let total = emu
                        .scroll_position()
                        .unwrap_or(None)
                        .map(|p| p.total)
                        .unwrap_or(rows as usize);
                    let top = emu
                        .viewport_offset()
                        .unwrap_or(total.saturating_sub(rows as usize));
                    let anchor = top + cursor_row.unwrap_or(rows as usize - 1);
                    let target = if up {
                        prompts.iter().rev().find(|&&r| r < anchor)
                    } else {
                        prompts.iter().find(|&&r| r > anchor)
                    };
                    let landed = match target {
                        Some(&row) => {
                            emu.scroll(ScrollTarget::Row(row));
                            Some(row)
                        }
                        None if up => {
                            // No prompt above: pin to the top like tmux.
                            emu.scroll(ScrollTarget::Top);
                            None
                        }
                        None => {
                            // No prompt below: back to the bottom (live).
                            emu.scroll(ScrollTarget::Bottom);
                            None
                        }
                    };
                    push_snapshot(id, &mut emu, cols, rows, &out);
                    // Report where the prompt landed relative to the
                    // new viewport top so the client puts its copy
                    // cursor on the prompt row.
                    let row = match landed {
                        Some(prompt_row) => {
                            let top_after = emu
                                .viewport_offset()
                                .unwrap_or(total.saturating_sub(rows as usize));
                            prompt_row.saturating_sub(top_after)
                        }
                        None => cursor_row.unwrap_or(rows as usize - 1),
                    };
                    let _ = out.send(PaneOut::PromptLanded { pane: id, row });
                }
                PaneCmd::YankCommand { anchor } => {
                    let text = emu.command_text(anchor).unwrap_or_default();
                    let _ = out.send(PaneOut::ScrollbackDump { pane: id, text });
                }
                PaneCmd::LoadScrollback { text } => {
                    // The editor's result replaces the pane's history:
                    // erase scrollback, pin to the bottom, and feed the
                    // text through the emulator so styling and prompt
                    // markers rebuild from the new content.
                    emu.clear_history();
                    emu.scroll(ScrollTarget::Bottom);
                    emu.feed(text.as_bytes());
                    for reply in emu.take_pty_writes() {
                        let _ = pty.write_all(&reply);
                    }
                    push_snapshot(id, &mut emu, cols, rows, &out);
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
        lines: emu.screen_lines().unwrap_or_default(),
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
                Ok(PaneOut::ScrollbackDump { .. }) => {}
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
