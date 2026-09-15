mod benchmark;
mod clipboard;
mod edit;
mod input;
mod render;
mod selection;

use corral_core::tree::PaneId;
use corrald::protocol::{ClientMsg, PaneState, ServerMsg};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(16);

// Upper bound on one drain pass over the socket. The drain loop exits on
// WouldBlock, but a daemon streaming frames continuously can keep it fed
// forever, starving the key poll and the draw below. The budget forces a
// yield so typed keys and rendering always make progress.
const DRAIN_BUDGET: Duration = Duration::from_millis(100);

fn socket_path() -> std::path::PathBuf {
    // corrald owns this helper; the client mirrors the env-over-UID rule.
    if let Ok(p) = std::env::var("CORRAL_SOCKET") {
        return std::path::PathBuf::from(p);
    }
    let uid = std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "0".into());
    std::env::temp_dir().join(format!("corral-{uid}.sock"))
}

/// The copy cursor's starting position on entering copy mode: the real
/// terminal cursor's row when visible, so the first `{` jumps past the
/// live prompt instead of landing on it (a themed prompt like Tide can
/// still be drawing its live prompt without a resolvable OSC 133;B
/// position, so anchoring at the viewport bottom re-finds it instead of
/// the previous completed command). Falls back to the viewport's bottom
/// row when the cursor is hidden (alt-screen programs, or a program
/// that conceals it).
fn initial_copy_cursor(pane_cursor: Option<(u16, u16)>, height: usize) -> (usize, usize) {
    match pane_cursor {
        Some((col, row)) => (row as usize, col as usize),
        None => (height.saturating_sub(1), 0),
    }
}

fn send_msg(stream: &mut UnixStream, msg: &ClientMsg) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    // The socket is nonblocking; a full send buffer returns WouldBlock,
    // which is retryable, not fatal. Retry briefly so a keystroke is not
    // dropped when the daemon is streaming frames.
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    let mut bytes = line.as_bytes();
    while !bytes.is_empty() {
        match stream.write(bytes) {
            Ok(0) => anyhow::bail!("write returned zero"),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() > deadline {
                    anyhow::bail!("socket send buffer stayed full for 1s");
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(e.into()),
        }
    }
    stream.flush()?;
    Ok(())
}

/// Read one ScrollbackDump reply synchronously, skipping frames that
/// arrive first. Puts the socket back in nonblocking mode afterwards.
fn read_dump(reader: &mut std::io::BufReader<UnixStream>) -> anyhow::Result<String> {
    // The socket runs nonblocking for the main loop; a read timeout on a
    // nonblocking socket never fires (EAGAIN wins), so restore blocking
    // mode first or every dump read returns immediately.
    reader.get_mut().set_nonblocking(false)?;
    reader
        .get_mut()
        .set_read_timeout(Some(Duration::from_secs(5)))?;
    let text = loop {
        let mut chunk = String::new();
        match std::io::BufRead::read_line(reader, &mut chunk) {
            Ok(0) => anyhow::bail!("daemon closed during scrollback dump"),
            Ok(_) => {}
            Err(e) => anyhow::bail!("no scrollback dump within 5s: {e}"),
        }
        if let Ok(ServerMsg::ScrollbackDump { text, .. }) = serde_json::from_str(chunk.trim()) {
            break text;
        }
    };
    reader.get_mut().set_read_timeout(None)?;
    reader.get_mut().set_nonblocking(true)?;
    Ok(text)
}

fn main() -> anyhow::Result<()> {
    let path = socket_path();
    let stream = UnixStream::connect(&path)?;
    stream.set_nonblocking(true)?;
    let mut writer = stream.try_clone()?;

    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    let _ = crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen);
    let result = run(stream, &mut writer);
    let _ = crossterm::execute!(stdout, crossterm::terminal::LeaveAlternateScreen);
    crossterm::terminal::disable_raw_mode()?;
    result
}

fn pane_command() -> (String, Vec<String>) {
    // CORRAL_SHELL wins over SHELL so a non-login-shell choice (fish) can
    // be set per-machine without changing the login shell.
    let shell = std::env::var("CORRAL_SHELL")
        .or_else(|_| std::env::var("SHELL"))
        .unwrap_or_else(|_| "/bin/sh".into());
    (shell, vec!["-l".into()])
}

fn run(stream: UnixStream, writer: &mut UnixStream) -> anyhow::Result<()> {
    send_msg(writer, &ClientMsg::Attach)?;
    // Size the daemon to the real terminal. The daemon answers Attach
    // with the current layout immediately, so this first read tells us
    // whether the session already has panes: rejoining must attach to
    // them, not spawn another shell.
    let (cols, rows) = crossterm::terminal::size()?;
    // Reserve the bottom row for the status bar: the daemon lays out
    // panes to fill whatever size it is told, so panes never draw into
    // the row render.rs paints the status line on.
    send_msg(
        writer,
        &ClientMsg::Resize {
            cols,
            rows: rows.saturating_sub(1),
        },
    )?;
    let mut reader = std::io::BufReader::new(stream);
    let mut first_frame: Option<Vec<PaneState>> = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    reader.get_mut().set_nonblocking(false)?;
    while first_frame.is_none() && std::time::Instant::now() < deadline {
        let mut line = String::new();
        match std::io::BufRead::read_line(&mut reader, &mut line) {
            Ok(0) => anyhow::bail!("daemon closed during attach"),
            Ok(_) => {
                if let Ok(ServerMsg::Frame { panes: p, .. }) =
                    serde_json::from_str::<ServerMsg>(line.trim())
                {
                    first_frame = Some(p);
                }
            }
            Err(e) => anyhow::bail!("no attach frame from daemon: {e}"),
        }
    }
    let Some(initial_panes) = first_frame else {
        anyhow::bail!("daemon sent no layout within 2s");
    };
    // The main loop drains the socket nonblocking; the attach read ran
    // blocking, so restore the mode it expects.
    reader.get_mut().set_nonblocking(true)?;
    if initial_panes.is_empty() {
        // Fresh session: the daemon starts at 80x24 with no panes, so
        // spawn the first shell now that the size is applied.
        let (shell, args) = pane_command();
        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
        send_msg(
            writer,
            &ClientMsg::CreatePane {
                cmd: shell,
                args,
                cwd,
                // The first pane fills the screen; the direction is unused.
                dir: corral_core::tree::Dir::Horizontal,
            },
        )?;
    }
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;
    let mut mode = input::Mode::Input;
    let mut selection: Option<selection::Selection> = None;
    // Copy-mode cursor: viewport-relative (row, col). Set on entering
    // copy mode, moved by hjkl, and placed on prompts by PromptLanded.
    let mut copy_cursor: Option<(usize, usize)> = None;
    // Search prompt state: the needle being typed, and the last submitted
    // needle that n/N repeat.
    let mut search_needle = String::new();
    let mut search_reverse = false;
    let mut last_search: Option<String> = None;
    let mut last_search_reverse = false;
    // The last search reply: hits as screen-space rows plus the
    // viewport top the daemon landed on, highlighted in the viewport
    // until a new search replaces them.
    let mut search_hits: Option<(PaneId, Vec<usize>, usize)> = None;
    // The real run uses the system clipboard; tests drive the run loop's
    // pieces with MemoryClipboard directly.
    let mut clipboard: Box<dyn clipboard::Clipboard> = Box::new(clipboard::SystemClipboard::new());
    let mut leader_armed = false;
    let mut buf = String::new();
    let mut panes: Vec<PaneState> = Vec::new();
    let mut focused: PaneId = 0;
    // Ratatui emits show-cursor plus a cursor move on every draw, and a
    // cursor move resets the terminal's blink timer. Redrawing only when
    // a frame actually differs keeps the blink alive while idle.
    // The selection cursor joins the diff key: a SelectMove changes no
    // pane state but must repaint the highlight.
    type FrameKey = (
        Vec<PaneState>,
        PaneId,
        render::Hint,
        Option<(usize, usize)>,
        Option<(usize, usize)>,
        Option<(PaneId, Vec<usize>, usize)>,
    );
    let mut last_drawn: Option<FrameKey> = None;
    loop {
        // Drain socket lines (nonblocking): frames land in the pane state
        // used by the draw below. A daemon streaming frames can keep this
        // loop fed forever, so a time budget bounds the pass and lets the
        // key poll and draw below run.
        let deadline = std::time::Instant::now() + DRAIN_BUDGET;
        loop {
            let mut chunk = String::new();
            match std::io::BufRead::read_line(&mut reader, &mut chunk) {
                Ok(0) => break,
                // EAGAIN can land after read_line consumed a partial
                // frame: the bytes are already in `chunk`, so dropping
                // it corrupts the stream (the frame's tail arrives on a
                // later pass and never parses). Keep the partial head.
                Err(_) => {
                    buf.push_str(&chunk);
                    break;
                }
                Ok(_) => {
                    buf.push_str(&chunk);
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                }
            }
        }
        while let Some(pos) = buf.find('\n') {
            let line: String = buf.drain(..=pos).collect();
            let Ok(msg) = serde_json::from_str::<ServerMsg>(line.trim()) else {
                continue;
            };
            match msg {
                ServerMsg::Frame {
                    panes: p,
                    focused: f,
                } => {
                    panes = p;
                    focused = f;
                }
                ServerMsg::Exited { .. } => {}
                ServerMsg::SearchResult { pane, rows, top } => {
                    // The worker already scrolled to the first match; the
                    // frame carrying the new viewport follows right
                    // behind. Hits stay highlighted until the next search.
                    search_hits = Some((pane, rows, top));
                }
                ServerMsg::PromptLanded { pane, row, col } => {
                    // The prompt's command text now sits at (row, col)
                    // in the viewport: put the copy cursor there. The
                    // frame carrying the new viewport follows behind
                    // this reply.
                    if pane == focused {
                        copy_cursor = Some((row, col));
                    }
                }
                ServerMsg::ScrollbackDump { .. } => {
                    // Only meaningful in the EditScrollback flow, which
                    // reads the socket directly; anything arriving in the
                    // normal loop is stale.
                }
            }
        }
        let focused_pane = panes.iter().find(|p| p.id == focused);
        let app_cursor = focused_pane.map(|p| p.app_cursor).unwrap_or(false);
        let half_page = focused_pane.map(|p| p.rect.h / 2).unwrap_or(0);
        if crossterm::event::poll(POLL)?
            && let crossterm::event::Event::Key(ev) = crossterm::event::read()?
        {
            let action = input::handle(ev, &mut leader_armed, &mode, app_cursor, half_page);
            match action {
                Some(input::Action::Quit) => break,
                Some(input::Action::EnterCopy) => {
                    mode = input::Mode::Copy;
                    let bottom = focused_pane.map(|p| p.rect.h as usize).unwrap_or(1);
                    copy_cursor = Some(initial_copy_cursor(
                        focused_pane.and_then(|p| p.cursor),
                        bottom,
                    ));
                }
                Some(input::Action::ExitCopy) => {
                    mode = input::Mode::Input;
                    selection = None;
                    copy_cursor = None;
                    search_hits = None;
                    // Leaving copy mode restores live follow at the bottom.
                    send_msg(
                        writer,
                        &ClientMsg::Scroll {
                            target: corrald::protocol::ScrollTarget::Bottom,
                        },
                    )?;
                }
                Some(input::Action::CopyCursorMove { drow, dcol }) => {
                    // Move the viewport cursor; when it pushes past the
                    // top or bottom edge, drag the viewport with it.
                    if let (Some(cur), Some(p)) = (copy_cursor.as_mut(), focused_pane) {
                        let height = p.rect.h as isize;
                        let width = p.rect.w as isize;
                        let (r, c) = (cur.0 as isize, cur.1 as isize);
                        let new_r = (r + drow).clamp(0, height - 1).max(0);
                        let new_c = (c + dcol).clamp(0, width - 1).max(0);
                        *cur = (new_r as usize, new_c as usize);
                        if new_r == 0 && drow < 0 {
                            send_msg(
                                writer,
                                &ClientMsg::Scroll {
                                    target: corrald::protocol::ScrollTarget::Delta(-1),
                                },
                            )?;
                        } else if new_r == height - 1 && drow > 0 {
                            send_msg(
                                writer,
                                &ClientMsg::Scroll {
                                    target: corrald::protocol::ScrollTarget::Delta(1),
                                },
                            )?;
                        }
                    }
                }
                Some(input::Action::CopyScroll(target)) => {
                    send_msg(writer, &ClientMsg::Scroll { target })?;
                    // g/G pin the copy cursor to the new viewport edge.
                    // Ctrl+u/Ctrl+d (Delta) scroll the content under a
                    // stationary cursor instead, like vim/tmux: the
                    // cursor's row inside the viewport does not change,
                    // so it stays centered rather than snapping to an
                    // edge.
                    let height = focused_pane.map(|p| p.rect.h as usize).unwrap_or(1);
                    match target {
                        corrald::protocol::ScrollTarget::Top => {
                            copy_cursor = Some((0, copy_cursor.map_or(0, |c| c.1)));
                        }
                        corrald::protocol::ScrollTarget::Bottom => {
                            copy_cursor = Some((height - 1, copy_cursor.map_or(0, |c| c.1)));
                        }
                        corrald::protocol::ScrollTarget::Delta(_) => {}
                        corrald::protocol::ScrollTarget::Row(_) => {}
                    }
                }
                Some(input::Action::BeginSelect(kind)) => {
                    // The anchor is the copy cursor: selection starts
                    // where the user is looking, not the viewport
                    // top-left. Copy mode always sets a cursor first.
                    let anchor = copy_cursor.unwrap_or((0, 0));
                    selection = Some(selection::Selection::start(kind, anchor));
                    mode = input::Mode::Select(kind);
                }
                Some(input::Action::SelectMove { drow, dcol }) => {
                    if let (Some(sel), Some(p)) = (selection.as_mut(), focused_pane) {
                        let grid = selection::Grid::from_text(&p.text);
                        sel.extend(&grid, drow, dcol);
                    }
                }
                Some(input::Action::Yank) => {
                    if let (Some(sel), Some(p)) = (selection.as_ref(), focused_pane) {
                        clipboard.set_text(&sel.text(&p.text))?;
                        selection = None;
                        mode = input::Mode::Copy;
                    }
                }
                Some(input::Action::CancelSelect) => {
                    selection = None;
                    mode = input::Mode::Copy;
                }
                Some(input::Action::BeginSearch) | Some(input::Action::BeginSearchReverse) => {
                    search_needle.clear();
                    // Remember the direction so Enter searches the way
                    // the prompt was opened; n/N keep repeating it.
                    search_reverse = matches!(action, Some(input::Action::BeginSearchReverse));
                    mode = input::Mode::Search;
                }
                Some(input::Action::SearchChar(c)) => search_needle.push(c),
                Some(input::Action::SearchBackspace) => {
                    search_needle.pop();
                }
                Some(input::Action::SearchSubmit) => {
                    if search_needle.is_empty() {
                        mode = input::Mode::Copy;
                    } else {
                        last_search = Some(search_needle.clone());
                        last_search_reverse = search_reverse;
                        // Forward search starts at the top of scrollback
                        // (first match after /); reverse starts from the
                        // bottom and walks up (first match above ?).
                        send_msg(
                            writer,
                            &ClientMsg::Search {
                                needle: search_needle.clone(),
                                from: None,
                                reverse: search_reverse,
                            },
                        )?;
                        mode = input::Mode::Copy;
                    }
                }
                Some(input::Action::SearchCancel) => mode = input::Mode::Copy,
                Some(input::Action::SearchNext) | Some(input::Action::SearchPrev) => {
                    if let Some(needle) = last_search.as_ref() {
                        // Resume from the viewport top: the daemon scrolls
                        // to the next hit at or after that row. n repeats
                        // in the direction the search was submitted with.
                        let from = focused_pane.and_then(|p| p.scroll).map(|s| s.offset);
                        let reverse = if matches!(action, Some(input::Action::SearchNext)) {
                            last_search_reverse
                        } else {
                            !last_search_reverse
                        };
                        send_msg(
                            writer,
                            &ClientMsg::Search {
                                needle: needle.clone(),
                                from,
                                reverse,
                            },
                        )?;
                    }
                }
                Some(input::Action::Focus(dir)) => {
                    send_msg(writer, &ClientMsg::Focus { dir })?;
                }
                Some(input::Action::FocusNext) => {
                    send_msg(writer, &ClientMsg::FocusNext)?;
                }
                Some(input::Action::ClearHistory) => {
                    send_msg(writer, &ClientMsg::ClearHistory)?;
                }
                Some(input::Action::PromptPrev) => {
                    send_msg(
                        writer,
                        &ClientMsg::PromptJump {
                            up: true,
                            cursor_row: copy_cursor.map(|(r, _)| r),
                        },
                    )?;
                }
                Some(input::Action::PromptNext) => {
                    send_msg(
                        writer,
                        &ClientMsg::PromptJump {
                            up: false,
                            cursor_row: copy_cursor.map(|(r, _)| r),
                        },
                    )?;
                }
                Some(input::Action::EditScrollback) => {
                    send_msg(writer, &ClientMsg::DumpScrollback { pane: None })?;
                    // A failed dump must not kill the client; the session
                    // stays usable and the error surfaces in the hint line.
                    let Ok(dump) = read_dump(&mut reader) else {
                        continue;
                    };
                    // Suspend the TUI, hand the dump to the editor, then
                    // restore; the dump file is deleted inside the flow.
                    let _ = crossterm::execute!(
                        std::io::stdout(),
                        crossterm::terminal::LeaveAlternateScreen
                    );
                    crossterm::terminal::disable_raw_mode()?;
                    let edit_result = edit::edit_scrollback(&dump, &mut edit::spawn_editor);
                    crossterm::terminal::enable_raw_mode()?;
                    let _ = crossterm::execute!(
                        std::io::stdout(),
                        crossterm::terminal::EnterAlternateScreen
                    );
                    // Force a full repaint: the editor scribbled on the
                    // screen behind ratatui's diff cache.
                    terminal.clear()?;
                    last_drawn = None;
                    if let Ok(edited) = edit_result {
                        send_msg(writer, &ClientMsg::LoadScrollback { text: edited })?;
                    }
                }
                Some(input::Action::Split(dir)) => {
                    let (cmd, args) = pane_command();
                    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
                    send_msg(
                        writer,
                        &ClientMsg::CreatePane {
                            cmd,
                            args,
                            cwd,
                            dir,
                        },
                    )?;
                }
                Some(input::Action::Send(bytes)) => {
                    send_msg(writer, &ClientMsg::Key { bytes })?;
                }
                None => {}
            }
        }
        let hint = match mode {
            input::Mode::Input => render::Hint::None,
            input::Mode::Copy => render::Hint::Copy(
                focused_pane
                    .and_then(|p| p.scroll)
                    .map(|s| (s.offset, s.total)),
            ),
            input::Mode::Select(_) => render::Hint::Select,
            input::Mode::Search => render::Hint::Search(search_needle.clone()),
        };
        let sel_cursor = selection.as_ref().map(|s| s.cursor);
        let visible_cursor = matches!(mode, input::Mode::Copy | input::Mode::Select(_))
            .then_some(copy_cursor)
            .flatten();
        let active_search = search_hits.as_ref().and_then(|(pane, rows, _top)| {
            let p = panes.iter().find(|p| p.id == *pane)?;
            let needle = last_search.as_ref()?;
            Some((*pane, render::search_spans(&p.text, needle)))
                .filter(|(_, spans)| !spans.is_empty() || !rows.is_empty())
        });
        // The current match paints differently from the rest: the
        // daemon's first hit row is the one the viewport jumped to, so
        // it maps to a viewport row by the same arithmetic the worker
        // used to land there.
        let current_hit: Option<(PaneId, render::SpanList)> =
            match (search_hits.as_ref(), last_search.as_ref()) {
                (Some((pane, rows, top)), Some(needle)) if !rows.is_empty() => {
                    let Some(p) = panes.iter().find(|p| p.id == *pane) else {
                        continue;
                    };
                    let hit_row = rows[0];
                    let row_in_view = hit_row.saturating_sub(*top);
                    if row_in_view < p.rect.h as usize {
                        Some((
                            *pane,
                            render::current_hit_spans(&p.text, needle, row_in_view),
                        ))
                    } else {
                        None
                    }
                }
                _ => None,
            };
        // The whole hint joins the diff key: collapsing it to a bool
        // would skip the repaint that reveals the search prompt when a
        // Copy frame turns into a Search frame.
        let frame_changed = last_drawn.as_ref()
            != Some(&(
                panes.clone(),
                focused,
                hint.clone(),
                sel_cursor,
                visible_cursor,
                search_hits.clone(),
            ));
        if frame_changed {
            last_drawn = Some((
                panes.clone(),
                focused,
                hint.clone(),
                sel_cursor,
                visible_cursor,
                search_hits.clone(),
            ));
            terminal.draw(|f| {
                // Selection spans render reversed over the focused pane's
                // visible grid.
                let spans: Vec<(PaneId, render::SpanList)> = match (&selection, focused_pane) {
                    (Some(sel), Some(p)) => vec![(p.id, sel.spans(&p.text))],
                    _ => Vec::new(),
                };
                // Search hits highlight in the pane the daemon matched;
                // spans are already viewport rows because the daemon
                // scrolled the hit onto screen before replying.
                let search: Vec<(PaneId, render::SpanList)> = active_search
                    .map(|(pane, spans)| vec![(pane, spans)])
                    .unwrap_or_default();
                // The current match paints over the yellow: a distinct
                // background marks which hit n would land on next.
                let current: Vec<(PaneId, render::SpanList)> = current_hit
                    .map(|(pane, spans)| vec![(pane, spans)])
                    .unwrap_or_default();
                render::draw(
                    f,
                    &panes,
                    focused,
                    hint,
                    &spans,
                    &search,
                    &current,
                    visible_cursor,
                );
                // Position the real cursor inside the frame. Full-screen
                // programs manage their own cursor.
                if let Some(p) = panes.iter().find(|p| p.id == focused)
                    && let Some((cx, cy)) = p.cursor
                {
                    let (x, y) = (p.rect.x + cx, p.rect.y + cy);
                    if x < p.rect.x + p.rect.w && y < p.rect.y + p.rect.h {
                        f.set_cursor_position(ratatui::layout::Position::new(x, y));
                    }
                }
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_prefers_the_env_override() {
        // SAFETY: single-threaded test env manipulation.
        unsafe { std::env::set_var("CORRAL_SOCKET", "/tmp/client-env-wins.sock") };
        assert_eq!(
            socket_path(),
            std::path::PathBuf::from("/tmp/client-env-wins.sock")
        );
        unsafe { std::env::remove_var("CORRAL_SOCKET") };
    }

    #[test]
    fn socket_path_falls_back_to_the_uid_shape() {
        let path = socket_path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("corral-"), "got {name}");
        assert!(name.ends_with(".sock"), "got {name}");
        let uid = name.trim_start_matches("corral-").trim_end_matches(".sock");
        assert!(
            !uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit()),
            "uid suffix {uid:?} is not numeric"
        );
    }

    #[test]
    fn send_msg_writes_a_terminated_json_line() {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut writer = a;
        send_msg(&mut writer, &ClientMsg::Attach).unwrap();
        drop(writer);
        let mut got = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(b), &mut got).unwrap();
        // Unit variants serialize as bare strings; the newline terminator
        // is what the JSON-lines framing depends on.
        assert_eq!(got, "\"Attach\"\n");
    }

    // The run loop's yank path: begin a selection at the grid origin,
    // extend with motions, yank, and confirm the memory clipboard holds
    // the exact text. Exercises the same Action arms the real loop runs.
    #[test]
    fn yank_via_selection_motions_captures_selected_text() {
        use clipboard::Clipboard as _;
        let text = "alpha\nbeta\ngamma\n";
        let mut clipboard = clipboard::MemoryClipboard::default();
        let grid = selection::Grid::from_text(text);
        let mut sel = selection::Selection::start(selection::SelectMode::Span, (0, 0));
        sel.extend(&grid, 1, 3);
        let yanked = sel.text(text);
        clipboard.set_text(&yanked).unwrap();
        assert_eq!(clipboard.text, "alpha\nbeta");
    }

    #[test]
    fn initial_copy_cursor_uses_the_real_pty_cursor_when_visible() {
        // (col, row) from the pane, as PaneState::cursor reports it.
        assert_eq!(initial_copy_cursor(Some((2, 10)), 24), (10, 2));
    }

    #[test]
    fn initial_copy_cursor_falls_back_to_the_viewport_bottom_when_hidden() {
        assert_eq!(initial_copy_cursor(None, 24), (23, 0));
    }
}
