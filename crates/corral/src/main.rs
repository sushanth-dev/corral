mod attach;
mod benchmark;
mod clipboard;
mod clock;
mod input;
mod render;
mod selection;
mod theme;

use corral_core::tree::{PaneId, Rect};
use corrald::protocol::{ClientMsg, PaneState, ServerMsg, WorkspaceInfo};
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

/// Where to put the terminal's native cursor this frame, or `None` to
/// leave it hidden. Only input mode positions it: copy and select mode
/// paint their own cursor cell (`render::paint_cursor` via
/// `visible_cursor`), so positioning the native cursor as well puts a
/// second one on screen at the live shell cursor's unrelated spot. The
/// client-drawn search prompt is the same case.
fn native_cursor_position(
    mode: &input::Mode,
    rect: Rect,
    pane_cursor: Option<(u16, u16)>,
) -> Option<(u16, u16)> {
    if !matches!(mode, input::Mode::Input) {
        return None;
    }
    let (cx, cy) = pane_cursor?;
    let (x, y) = (rect.x + cx, rect.y + cy);
    (x < rect.x + rect.w && y < rect.y + rect.h).then_some((x, y))
}

/// The copy cursor's row after a Ctrl+u/Ctrl+d half-page scroll.
///
/// The scroll lands the cursor on the pane's middle row, not where it
/// started: copy mode's cursor is a reading position on the pane, and
/// half-page scrolling is only useful if the content just scrolled to
/// has room above and below it. With scrollback left to travel the
/// viewport delivers the full request and the cursor sits on that
/// middle row while the content slides under it. At either end the
/// viewport clamps, so the cursor takes up the rows it could not
/// deliver; holding it still there would stick it to the edge the
/// viewport pinned against (S3-3).
fn copy_cursor_row_after_half_page_scroll(requested: isize, moved: isize, height: usize) -> usize {
    let row = (height / 2) as isize + requested - moved;
    row.clamp(0, height as isize - 1).max(0) as usize
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

/// What the command line asked for.
#[derive(Debug, PartialEq)]
enum Request {
    /// No arguments: attach to the local daemon without asking, the
    /// behavior every earlier version had.
    Attach,
    /// `corral attach`: list what is running and attach to the choice.
    Pick,
    /// `corral attach <id>`: attach to the named workspace.
    Workspace(String),
}

fn parse_args(args: impl Iterator<Item = String>) -> anyhow::Result<Request> {
    let args: Vec<String> = args.collect();
    match args.as_slice() {
        [] => Ok(Request::Attach),
        [cmd] if cmd == "attach" => Ok(Request::Pick),
        [cmd, id] if cmd == "attach" => Ok(Request::Workspace(id.clone())),
        _ => anyhow::bail!("usage: corral [attach [workspace]]"),
    }
}

/// Ask a daemon what it is serving, over a connection of its own: a
/// listing is a query, not a session, so it is opened and dropped before
/// the real attach.
fn list_workspaces(path: &std::path::Path) -> anyhow::Result<Vec<WorkspaceInfo>> {
    let stream = UnixStream::connect(path)?;
    // Blocking, with a deadline: this is a one-shot query, and a daemon
    // that never answers should not hang the client forever.
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = std::io::BufReader::new(stream);
    send_msg(&mut writer, &ClientMsg::ListWorkspaces)?;
    let mut line = String::new();
    loop {
        line.clear();
        match std::io::BufRead::read_line(&mut reader, &mut line) {
            Ok(0) => anyhow::bail!("the daemon closed while listing workspaces"),
            Ok(_) => {}
            Err(e) => anyhow::bail!("no workspace list from the daemon: {e}"),
        }
        // A daemon sends a frame on connect, so the listing is not
        // necessarily the first line back.
        if let Ok(ServerMsg::WorkspaceList { workspaces }) = serde_json::from_str(line.trim()) {
            return Ok(workspaces);
        }
    }
}

/// Refuse to attach to a workspace this daemon does not serve. An id the
/// user typed can be stale, and attaching to some other workspace than the
/// one asked for is the wrong answer to a wrong id.
fn ensure_known(known: &[WorkspaceInfo], wanted: &str) -> anyhow::Result<()> {
    if known.iter().any(|w| w.id == wanted) {
        return Ok(());
    }
    let ids: Vec<&str> = known.iter().map(|w| w.id.as_str()).collect();
    anyhow::bail!(
        "no workspace {wanted:?}; this daemon serves {}",
        ids.join(", ")
    )
}

fn main() -> anyhow::Result<()> {
    let request = parse_args(std::env::args().skip(1))?;
    let path = socket_path();
    // The status bar's left zone. Resolved before the screen is taken
    // over, so the bar never has an empty workspace name.
    let workspace = workspace_id(&path);

    // The picker runs before the screen is taken over: fzf and the
    // numbered prompt both draw on the plain terminal, and neither works
    // inside the alternate screen.
    match &request {
        Request::Attach => {}
        Request::Workspace(id) => ensure_known(&list_workspaces(&path)?, id)?,
        Request::Pick => {
            // The choice comes out of the listing, so it names a
            // workspace this daemon just reported.
            let listing = list_workspaces(&path)?;
            if attach::choose(&listing).is_none() {
                eprintln!("corral: no workspace chosen");
                return Ok(());
            }
        }
    }

    // Resolved before the screen is taken over: a load failure warns on the
    // plain terminal rather than scrolling under the alternate screen.
    let theme = theme::Theme::resolve();

    let stream = UnixStream::connect(&path)?;
    stream.set_nonblocking(true)?;
    let mut writer = stream.try_clone()?;

    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    let _ = crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen);
    let _ = crossterm::execute!(stdout, crossterm::event::EnableMouseCapture);
    let result = run(stream, &mut writer, &workspace, &theme);
    let _ = crossterm::execute!(stdout, crossterm::event::DisableMouseCapture);
    let _ = crossterm::execute!(stdout, crossterm::terminal::LeaveAlternateScreen);
    crossterm::terminal::disable_raw_mode()?;
    result
}

fn pane_command() -> (String, Vec<String>) {
    // The user's own login shell, run as a login shell so their profile is
    // read. No override: one shell per machine keeps the panes identical to
    // the terminal the client was launched from.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    (shell, vec!["-l".into()])
}

/// The workspace id the daemon serves on the socket the client connected
/// to. Both ends derive it from the same path (see `Daemon::workspace_id`),
/// so the client reads it back for the status bar without asking.
fn workspace_id(path: &std::path::Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn run(
    stream: UnixStream,
    writer: &mut UnixStream,
    workspace: &str,
    theme: &theme::Theme,
) -> anyhow::Result<()> {
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
    // The leader's `t` shortcut opens the keymap dialogue over the panes.
    // Off by default: it is something to consult, not something to sit in
    // front of.
    let mut keymaps = false;
    // The status bar shortens the pane's directory against `$HOME`, so a
    // path under home reads as `~`. Read once, since the environment does
    // not change under us.
    let home = std::env::var("HOME").unwrap_or_default();
    // The status bar's clock, re-read at most once per its refresh window
    // so the render stays free of the wall clock.
    let mut clock = clock::Clock::new();
    let mut selection: Option<selection::Selection> = None;
    // Copy-mode cursor: viewport-relative (row, col). Set on entering
    // copy mode, moved by hjkl, and placed on prompts by PromptLanded.
    let mut copy_cursor: Option<(usize, usize)> = None;
    // The delta of the half-page scroll awaiting its ScrollLanded reply.
    // The client applies the reply before reading the next key, so one
    // slot is enough: nothing can move the cursor in between.
    let mut copy_scroll_pending: Option<isize> = None;
    // Set by a wheel tick toward the live screen: copy mode ends when the
    // frame that follows reports the viewport live again. A flag rather
    // than a check on the frame alone, because entering copy mode at the
    // bottom of the scrollback also reads as live and must not exit.
    let mut exit_copy_when_live = false;
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
    // The gutter a mouse-down started tracking, if any; carried across
    // the drag events that follow it.
    let mut drag: Option<(PaneId, corral_core::tree::Dir, Rect)> = None;
    let mut buf = String::new();
    let mut panes: Vec<PaneState> = Vec::new();
    let mut focused: PaneId = 0;
    // Ratatui emits show-cursor plus a cursor move on every draw, and a
    // cursor move resets the terminal's blink timer. Redrawing only when
    // a frame actually differs keeps the blink alive while idle.
    // The selection cursor joins the diff key: a SelectMove changes no
    // pane state but must repaint the highlight. The clock's text joins
    // it for the same reason, so the bar's minute advances on an
    // otherwise idle screen. The bool is the keymap dialogue's state; its
    // contents derive from the hint already in the key.
    type FrameKey = (
        Vec<PaneState>,
        PaneId,
        render::Hint,
        bool,
        Option<(usize, usize)>,
        Option<(usize, usize)>,
        Option<(PaneId, Vec<usize>, usize)>,
        String,
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
                    // A wheel tick that scrolled back to the live screen
                    // ends copy mode, so the bar and the keys return to
                    // input mode instead of staying in a mode whose
                    // viewport no longer exists.
                    if exit_copy_when_live
                        && panes
                            .iter()
                            .find(|p| p.id == focused)
                            .is_some_and(|p| p.scroll.is_none())
                    {
                        mode = input::Mode::Input;
                        selection = None;
                        copy_cursor = None;
                        copy_scroll_pending = None;
                        search_hits = None;
                        exit_copy_when_live = false;
                    }
                }
                ServerMsg::Exited { .. } => {}
                // Only the listing connection asks for this, so the run
                // loop cannot see one. It is here because the reply enum
                // is shared by both connections.
                ServerMsg::WorkspaceList { .. } => {}
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
                ServerMsg::ScrollLanded { pane, moved } => {
                    // Half-page scroll: put the cursor on the pane's
                    // middle row, less the rows the viewport could not
                    // follow. The reply always arrives before the next
                    // key, so the cursor is still where the scroll left
                    // it.
                    if pane == focused
                        && let (Some(requested), Some(cur)) =
                            (copy_scroll_pending.take(), copy_cursor)
                    {
                        let height = panes
                            .iter()
                            .find(|p| p.id == pane)
                            .map(|p| p.rect.h as usize)
                            .unwrap_or(1);
                        copy_cursor = Some((
                            copy_cursor_row_after_half_page_scroll(requested, moved, height),
                            cur.1,
                        ));
                    }
                }
            }
        }
        let focused_pane = panes.iter().find(|p| p.id == focused);
        let app_cursor = focused_pane.map(|p| p.app_cursor).unwrap_or(false);
        let half_page = focused_pane.map(|p| p.rect.h / 2).unwrap_or(0);
        if crossterm::event::poll(POLL)? {
            match crossterm::event::read()? {
                crossterm::event::Event::Mouse(ev) => {
                    // The dialogue is modal: a click or a wheel tick while
                    // it is up would act on a pane the user cannot see.
                    if keymaps {
                        continue;
                    }
                    match input::handle_mouse(ev, &panes, &mode, focused, &mut drag) {
                        Some(input::MouseAction::FocusPane(pane)) => {
                            send_msg(writer, &ClientMsg::FocusPane { pane })?;
                        }
                        Some(input::MouseAction::SetSplitRatio { at, ratio }) => {
                            send_msg(writer, &ClientMsg::SetSplitRatio { at, ratio })?;
                        }
                        Some(input::MouseAction::Scroll { pane, delta }) => {
                            // A wheel tick toward the live screen leaves copy
                            // mode once the viewport is live again; the frame
                            // that follows says whether it got there. The
                            // cursor is left where the wheel found it: the
                            // mid-row repositioning belongs to the half-page
                            // keys, not to every tick. Scoped to the pane the
                            // wheel actually hit, so a tick over an
                            // unfocused pane cannot exit copy mode on the
                            // focused one.
                            exit_copy_when_live = delta > 0 && pane == focused;
                            send_msg(
                                writer,
                                &ClientMsg::Scroll {
                                    pane,
                                    target: corrald::protocol::ScrollTarget::Delta(delta),
                                },
                            )?;
                        }
                        Some(input::MouseAction::EnterCopyThenScroll { pane, delta }) => {
                            mode = input::Mode::Copy;
                            let target = panes.iter().find(|p| p.id == pane);
                            let height = target.map(|p| p.rect.h as usize).unwrap_or(1);
                            copy_cursor =
                                Some(initial_copy_cursor(target.and_then(|p| p.cursor), height));
                            send_msg(
                                writer,
                                &ClientMsg::Scroll {
                                    pane,
                                    target: corrald::protocol::ScrollTarget::Delta(delta),
                                },
                            )?;
                        }
                        None => {}
                    }
                    continue;
                }
                crossterm::event::Event::Key(ev) => {
                    // A pending wheel exit only covers the frames that
                    // follow its tick; a keystroke in between means the
                    // user is doing something else.
                    exit_copy_when_live = false;
                    // The dialogue is modal: any key closes it and reaches
                    // nothing else, so a key pressed while reading the list
                    // cannot also act on the session.
                    let action = if keymaps {
                        keymaps = false;
                        None
                    } else {
                        input::handle(ev, &mut leader_armed, &mode, app_cursor, half_page)
                    };
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
                            copy_scroll_pending = None;
                            search_hits = None;
                            // Leaving copy mode restores live follow at the bottom.
                            send_msg(
                                writer,
                                &ClientMsg::Scroll {
                                    pane: focused,
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
                                            pane: focused,
                                            target: corrald::protocol::ScrollTarget::Delta(-1),
                                        },
                                    )?;
                                } else if new_r == height - 1 && drow > 0 {
                                    send_msg(
                                        writer,
                                        &ClientMsg::Scroll {
                                            pane: focused,
                                            target: corrald::protocol::ScrollTarget::Delta(1),
                                        },
                                    )?;
                                }
                            }
                        }
                        Some(input::Action::CopyScroll(target)) => {
                            send_msg(
                                writer,
                                &ClientMsg::Scroll {
                                    pane: focused,
                                    target,
                                },
                            )?;
                            // g/G pin the copy cursor to the new viewport edge.
                            // Ctrl+u/Ctrl+d (Delta) scroll the content under a
                            // cursor that lands on the pane's middle row instead:
                            // with scrollback left to travel the viewport keeps up
                            // and the cursor holds that row, but at either end the
                            // viewport clamps and the cursor must take up the
                            // slack, or it sticks to the pinned edge (S3-3). The
                            // reply says how far the viewport really moved.
                            let height = focused_pane.map(|p| p.rect.h as usize).unwrap_or(1);
                            match target {
                                corrald::protocol::ScrollTarget::Top => {
                                    copy_scroll_pending = None;
                                    copy_cursor = Some((0, copy_cursor.map_or(0, |c| c.1)));
                                }
                                corrald::protocol::ScrollTarget::Bottom => {
                                    copy_scroll_pending = None;
                                    copy_cursor =
                                        Some((height - 1, copy_cursor.map_or(0, |c| c.1)));
                                }
                                corrald::protocol::ScrollTarget::Delta(d) => {
                                    copy_scroll_pending = Some(d);
                                }
                                corrald::protocol::ScrollTarget::Row(_) => {
                                    copy_scroll_pending = None;
                                }
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
                        Some(input::Action::BeginSearch)
                        | Some(input::Action::BeginSearchReverse) => {
                            search_needle.clear();
                            // Remember the direction so Enter searches the way
                            // the prompt was opened; n/N keep repeating it.
                            search_reverse =
                                matches!(action, Some(input::Action::BeginSearchReverse));
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
                        Some(input::Action::ToggleHint) => {
                            keymaps = !keymaps;
                        }
                        None => {}
                    }
                }
                _ => {}
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
        let clock_text = clock.text().to_string();
        let frame_changed = last_drawn.as_ref()
            != Some(&(
                panes.clone(),
                focused,
                hint.clone(),
                keymaps,
                sel_cursor,
                visible_cursor,
                search_hits.clone(),
                clock_text.clone(),
            ));
        if frame_changed {
            last_drawn = Some((
                panes.clone(),
                focused,
                hint.clone(),
                keymaps,
                sel_cursor,
                visible_cursor,
                search_hits.clone(),
                clock_text.clone(),
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
                    keymaps,
                    &spans,
                    &search,
                    &current,
                    visible_cursor,
                    workspace,
                    &home,
                    &clock_text,
                    theme,
                );
                // Position the real cursor inside the frame. Full-screen
                // programs manage their own cursor; copy and select mode
                // paint their own.
                if let Some(p) = panes.iter().find(|p| p.id == focused)
                    && let Some((x, y)) = native_cursor_position(&mode, p.rect, p.cursor)
                {
                    f.set_cursor_position(ratatui::layout::Position::new(x, y));
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

    #[test]
    fn half_page_scroll_centers_the_cursor_when_the_viewport_keeps_up() {
        // 20-row pane, half page 10, plenty of scrollback: the viewport
        // moves the full 10 and the cursor lands on the middle row,
        // wherever it was before the scroll.
        assert_eq!(copy_cursor_row_after_half_page_scroll(-10, -10, 20), 10);
        assert_eq!(copy_cursor_row_after_half_page_scroll(10, 10, 20), 10);
    }

    #[test]
    fn half_page_scroll_moves_the_cursor_by_the_rows_the_viewport_could_not() {
        // Scrollback shorter than a half page (5 rows against a 10-row
        // request): the viewport clamps after 5, so the cursor takes the
        // remaining 5 rather than staying on the middle row.
        assert_eq!(copy_cursor_row_after_half_page_scroll(-10, -5, 20), 5);
    }

    #[test]
    fn half_page_scroll_clamps_the_cursor_inside_the_viewport() {
        // Ctrl+d at the live prompt: the viewport cannot move at all, so
        // all 10 rows land on the cursor, which stops at the last row.
        assert_eq!(copy_cursor_row_after_half_page_scroll(10, 0, 20), 19);
        // Same at the top edge, going the other way.
        assert_eq!(copy_cursor_row_after_half_page_scroll(-10, 0, 20), 0);
    }

    #[test]
    fn native_cursor_is_positioned_only_in_input_mode() {
        let rect = Rect {
            x: 2,
            y: 3,
            w: 80,
            h: 24,
        };
        // Input mode: the pane's live shell cursor, offset into the pane's
        // rect.
        assert_eq!(
            native_cursor_position(&input::Mode::Input, rect, Some((4, 5))),
            Some((6, 8))
        );
        // Copy and select mode paint their own cursor cell, so the native
        // one would show up twice; the search prompt draws its own line.
        for mode in [
            input::Mode::Copy,
            input::Mode::Select(selection::SelectMode::Span),
            input::Mode::Search,
        ] {
            assert_eq!(
                native_cursor_position(&mode, rect, Some((4, 5))),
                None,
                "{mode:?} must not position the native cursor"
            );
        }
    }

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    #[test]
    fn no_arguments_attaches_without_asking() {
        // The v0.1 behavior: bare `corral` goes straight to the daemon.
        assert_eq!(parse_args(args(&[]).into_iter()).unwrap(), Request::Attach);
    }

    #[test]
    fn attach_alone_is_the_picker_and_attach_with_an_id_is_direct() {
        assert_eq!(
            parse_args(args(&["attach"]).into_iter()).unwrap(),
            Request::Pick
        );
        assert_eq!(
            parse_args(args(&["attach", "corral-501"]).into_iter()).unwrap(),
            Request::Workspace("corral-501".into())
        );
    }

    #[test]
    fn an_unsupported_command_line_is_a_usage_error() {
        for words in [
            vec!["detach"],
            vec!["attach", "corral-501", "extra"],
            vec!["", "x"],
        ] {
            assert!(
                parse_args(args(&words).into_iter()).is_err(),
                "{words:?} must not parse"
            );
        }
    }

    #[test]
    fn a_requested_workspace_must_be_one_the_daemon_serves() {
        let known = vec![WorkspaceInfo {
            id: "corral-501".into(),
            panes: 2,
            clients: 1,
        }];
        assert!(ensure_known(&known, "corral-501").is_ok());
        // A stale id, or one from another machine's daemon, is refused
        // rather than silently attaching to whatever is here.
        let err = ensure_known(&known, "corral-999").unwrap_err().to_string();
        assert!(err.contains("corral-999"), "got {err}");
        assert!(err.contains("corral-501"), "got {err}");
    }
}
