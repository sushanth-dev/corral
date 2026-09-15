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
    /// Jump the viewport to the previous (up) or next (down) OSC133
    /// prompt row (S3-8). `cursor_row` anchors the walk at the copy
    /// cursor inside the viewport; `None` anchors at the viewport
    /// bottom.
    PromptJump { up: bool, cursor_row: Option<usize> },
    /// Rebuild and resend the snapshot even if nothing changed.
    #[allow(dead_code)]
    Render,
}

pub enum PaneOut {
    Snapshot {
        pane: PaneId,
        state: crate::protocol::PaneState,
    },
    /// Reply to PaneCmd::Search; rows are screen-space row indexes, top
    /// is the viewport top after scrolling to the current hit.
    SearchResult {
        pane: PaneId,
        rows: Vec<usize>,
        top: usize,
    },
    /// Reply to PaneCmd::PromptJump: the prompt's command text landed
    /// at this row and column inside the viewport.
    PromptLanded {
        pane: PaneId,
        row: usize,
        col: usize,
    },
    /// Reply to PaneCmd::Scroll: how far the viewport actually moved,
    /// signed like ScrollTarget::Delta. A clamped scroll moves less.
    ScrollLanded {
        pane: PaneId,
        moved: isize,
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
                    // Measure the shift with viewport_offset(), which
                    // reports the real top even when the viewport is
                    // pinned to the bottom; scroll_position() collapses
                    // that case to None, so the client cannot recover
                    // the movement from the frame alone (S3-3).
                    let before = emu.viewport_offset().unwrap_or(0) as isize;
                    emu.scroll(target);
                    let after = emu.viewport_offset().unwrap_or(0) as isize;
                    let _ = out.send(PaneOut::ScrollLanded {
                        pane: id,
                        moved: after - before,
                    });
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
                    // Report the true viewport top alongside the hits:
                    // scroll_position() returns None when the viewport
                    // is pinned to the bottom, which the client can't
                    // tell apart from "viewport top is 0".
                    // viewport_offset() always reflects the real top.
                    let top = emu.viewport_offset().unwrap_or(0);
                    let _ = out.send(PaneOut::SearchResult {
                        pane: id,
                        rows: hits,
                        top,
                    });
                }
                PaneCmd::ClearHistory => {
                    // Erase scrollback only (3J); the live screen and
                    // whatever the user has typed at the prompt stay
                    // exactly as they are.
                    emu.clear_history();
                    push_snapshot(id, &mut emu, cols, rows, &out);
                }
                PaneCmd::PromptJump { up, cursor_row } => {
                    // Command input positions, not raw OSC 133;A marker
                    // rows: a shell theme (Tide) draws a decorative box
                    // around the marker row, so jumping to the marker
                    // lands on the box instead of the typed command.
                    // prompt_input_positions resolves the exact
                    // (row, col) of each command via OSC 133;B.
                    let prompts = emu.prompt_input_positions().unwrap_or_default();
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
                    let target = resolve_prompt_jump(&prompts, anchor, up);
                    let landed = match target {
                        Some((row, col)) => {
                            emu.scroll(ScrollTarget::Row(row));
                            Some((row, col))
                        }
                        None if up => match prompts.first() {
                            // Already on the first command: stay put
                            // rather than overshooting into blank space
                            // above it.
                            Some(&(row, col)) => {
                                emu.scroll(ScrollTarget::Row(row));
                                Some((row, col))
                            }
                            None => {
                                emu.scroll(ScrollTarget::Top);
                                None
                            }
                        },
                        None => {
                            // No prompt below: back to the bottom (live).
                            emu.scroll(ScrollTarget::Bottom);
                            None
                        }
                    };
                    push_snapshot(id, &mut emu, cols, rows, &out);
                    // Report where the prompt landed relative to the
                    // new viewport top so the client puts its copy
                    // cursor on the command's row and column.
                    let (row, col) = match landed {
                        Some((prompt_row, prompt_col)) => {
                            let top_after = emu
                                .viewport_offset()
                                .unwrap_or(total.saturating_sub(rows as usize));
                            (prompt_row.saturating_sub(top_after), prompt_col as usize)
                        }
                        None => (cursor_row.unwrap_or(rows as usize - 1), 0),
                    };
                    let _ = out.send(PaneOut::PromptLanded { pane: id, row, col });
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
                Ok(PaneOut::ScrollLanded { .. }) => {}
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
