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

fn send_msg(stream: &mut UnixStream, msg: &ClientMsg) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// Read one ScrollbackDump reply synchronously, skipping frames that
/// arrive first. Puts the socket back in nonblocking mode afterwards.
fn read_dump(reader: &mut std::io::BufReader<UnixStream>) -> anyhow::Result<String> {
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
    // Size the daemon to the real terminal and spawn the first shell; the
    // daemon starts at 80x24 and never resizes until told.
    let (cols, rows) = crossterm::terminal::size()?;
    let (shell, args) = pane_command();
    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
    send_msg(writer, &ClientMsg::Resize { cols, rows })?;
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
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;
    let mut mode = input::Mode::Input;
    let mut selection: Option<selection::Selection> = None;
    // Search prompt state: the needle being typed, and the last submitted
    // needle that n/N repeat.
    let mut search_needle = String::new();
    let mut last_search: Option<String> = None;
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
    type FrameKey = (Vec<PaneState>, PaneId, bool, Option<(usize, usize)>);
    let mut last_drawn: Option<FrameKey> = None;
    let mut reader = std::io::BufReader::new(stream);
    loop {
        // Drain socket lines (nonblocking): frames land in the pane state
        // used by the draw below.
        loop {
            let mut chunk = String::new();
            match std::io::BufRead::read_line(&mut reader, &mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(_) => buf.push_str(&chunk),
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
                ServerMsg::SearchResult { .. } => {
                    // The worker already scrolled to the match; the frame
                    // carrying the new viewport follows right behind. No
                    // highlight in v0.2 (plan Task 5 known gap).
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
                Some(input::Action::EnterCopy) => mode = input::Mode::Copy,
                Some(input::Action::ExitCopy) => {
                    mode = input::Mode::Input;
                    selection = None;
                    // Leaving copy mode restores live follow at the bottom.
                    send_msg(
                        writer,
                        &ClientMsg::Scroll {
                            target: corrald::protocol::ScrollTarget::Bottom,
                        },
                    )?;
                }
                Some(input::Action::CopyScroll(target)) => {
                    send_msg(writer, &ClientMsg::Scroll { target })?;
                }
                Some(input::Action::BeginSelect(kind)) => {
                    // The anchor is the top-left of the visible grid.
                    selection = Some(selection::Selection::start(kind, (0, 0)));
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
                Some(input::Action::BeginSearch) => {
                    search_needle.clear();
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
                        // Forward search starts at the top of scrollback so
                        // Enter always finds the first match after /.
                        send_msg(
                            writer,
                            &ClientMsg::Search {
                                needle: search_needle.clone(),
                                from: None,
                                reverse: false,
                            },
                        )?;
                        mode = input::Mode::Copy;
                    }
                }
                Some(input::Action::SearchCancel) => mode = input::Mode::Copy,
                Some(input::Action::SearchNext) | Some(input::Action::SearchPrev) => {
                    if let Some(needle) = last_search.as_ref() {
                        // Resume from the viewport top: the daemon scrolls
                        // to the next hit at or after that row.
                        let from = focused_pane.and_then(|p| p.scroll).map(|s| s.offset);
                        send_msg(
                            writer,
                            &ClientMsg::Search {
                                needle: needle.clone(),
                                from,
                                reverse: matches!(action, Some(input::Action::SearchPrev)),
                            },
                        )?;
                    }
                }
                Some(input::Action::Focus(dir)) => {
                    send_msg(writer, &ClientMsg::Focus { dir })?;
                }
                Some(input::Action::ClearHistory) => {
                    send_msg(writer, &ClientMsg::ClearHistory)?;
                }
                Some(input::Action::PromptPrev) => {
                    send_msg(writer, &ClientMsg::PromptJump { up: true })?;
                }
                Some(input::Action::PromptNext) => {
                    send_msg(writer, &ClientMsg::PromptJump { up: false })?;
                }
                Some(input::Action::EditScrollback) => {
                    send_msg(writer, &ClientMsg::DumpScrollback { pane: None })?;
                    let dump = read_dump(&mut reader)?;
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
                    edit_result?;
                }
                Some(input::Action::YankCommand) => {
                    // The viewport top anchors the block: `c` copies the
                    // command visible above the current view.
                    let anchor = focused_pane.and_then(|p| p.scroll).map(|s| s.offset);
                    send_msg(writer, &ClientMsg::YankCommand { anchor })?;
                    let text = read_dump(&mut reader)?;
                    clipboard.set_text(&text)?;
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
        let frame_changed = last_drawn.as_ref()
            != Some(&(
                panes.clone(),
                focused,
                hint != render::Hint::None,
                sel_cursor,
            ));
        if frame_changed {
            last_drawn = Some((
                panes.clone(),
                focused,
                hint != render::Hint::None,
                sel_cursor,
            ));
            terminal.draw(|f| {
                // Selection spans render reversed over the focused pane's
                // visible grid.
                let spans: Vec<(PaneId, render::SpanList)> = match (&selection, focused_pane) {
                    (Some(sel), Some(p)) => vec![(p.id, sel.spans(&p.text))],
                    _ => Vec::new(),
                };
                render::draw(f, &panes, focused, hint, &spans);
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
}
