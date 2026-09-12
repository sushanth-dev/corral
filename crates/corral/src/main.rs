mod benchmark;
mod input;
mod render;

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
        },
    )?;
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;
    let mut leader_armed = false;
    let mut buf = String::new();
    let mut panes: Vec<PaneState> = Vec::new();
    let mut focused: PaneId = 0;
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
            }
        }
        if crossterm::event::poll(POLL)?
            && let crossterm::event::Event::Key(ev) = crossterm::event::read()?
        {
            match input::handle(ev, &mut leader_armed) {
                Some(input::Action::Quit) => break,
                Some(input::Action::Focus(dir)) => {
                    send_msg(writer, &ClientMsg::Focus { dir })?;
                }
                Some(input::Action::Split(_)) => {
                    let (cmd, args) = pane_command();
                    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
                    send_msg(writer, &ClientMsg::CreatePane { cmd, args, cwd })?;
                }
                Some(input::Action::Send(bytes)) => {
                    send_msg(writer, &ClientMsg::Key { bytes })?;
                }
                None => {}
            }
        }
        terminal.draw(|f| render::draw(f, &panes, focused))?;
        // Show the real cursor at the focused pane's position; ratatui
        // hides it otherwise. Full-screen programs manage their own.
        if let Some(p) = panes.iter().find(|p| p.id == focused)
            && let Some((cx, cy)) = p.cursor
        {
            let (x, y) = (p.rect.x + cx, p.rect.y + cy);
            if x < p.rect.x + p.rect.w && y < p.rect.y + p.rect.h {
                terminal.show_cursor()?;
                terminal.set_cursor_position(ratatui::layout::Position::new(x, y))?;
            }
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
}
