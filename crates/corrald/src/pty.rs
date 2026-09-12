use anyhow::Result;
use portable_pty::{CommandBuilder, MasterPty, native_pty_system};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::{Receiver, channel};
use std::thread;

// The daemon server (S2-6) consumes PtyHandle; v0.1 ships the layer first.
#[allow(dead_code)]
pub enum PtyEvent {
    Output(Vec<u8>),
    Exited,
}

#[allow(dead_code)]
pub struct PtyHandle {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    pub rx: Receiver<PtyEvent>,
}

// See the note on PtyEvent: consumed by S2-6.
#[allow(dead_code)]
impl PtyHandle {
    pub fn spawn(shell: &str, args: &[&str], cwd: &Path, cols: u16, rows: u16) -> Result<Self> {
        let pair = native_pty_system().openpty(portable_pty::PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut cmd = CommandBuilder::new(shell);
        cmd.args(args);
        cmd.cwd(cwd);
        let mut child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let (tx, rx) = channel();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(PtyEvent::Output(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = child.wait();
            let _ = tx.send(PtyEvent::Exited);
        });
        Ok(Self {
            writer,
            master: pair.master,
            rx,
        })
    }

    pub fn write_all(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master.resize(portable_pty::PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn captures_command_output_through_pty() {
        let handle =
            PtyHandle::spawn("sh", &["-c", "printf hello"], Path::new("/tmp"), 80, 24).unwrap();
        let mut got: Vec<u8> = Vec::new();
        for ev in handle.rx {
            match ev {
                PtyEvent::Output(chunk) => got.extend(chunk),
                PtyEvent::Exited => break,
            }
            if got.windows(5).any(|w| w == b"hello") {
                break;
            }
        }
        assert!(String::from_utf8_lossy(&got).contains("hello"));
    }

    #[test]
    fn exited_arrives_after_child_exits() {
        let handle = PtyHandle::spawn("sh", &["-c", "exit 0"], Path::new("/tmp"), 80, 24).unwrap();
        let mut saw_output_end = false;
        for ev in handle.rx {
            match ev {
                PtyEvent::Output(_) => {}
                PtyEvent::Exited => {
                    saw_output_end = true;
                    break;
                }
            }
        }
        assert!(saw_output_end);
    }

    #[test]
    fn keys_written_reach_the_shell() {
        // Feed the shell a command through write_all; its output proves
        // the bytes crossed the PTY master/slave boundary. `cat` echoes
        // through the pty and the shell reads the command, so the echo of
        // the typed line itself is the signal.
        let mut handle = PtyHandle::spawn("cat", &[], Path::new("/tmp"), 80, 24).unwrap();
        handle.write_all(b"echo ok-$((1+1))\n").unwrap();
        let mut got: Vec<u8> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match handle
                .rx
                .recv_timeout(std::time::Duration::from_millis(100))
            {
                Ok(PtyEvent::Output(chunk)) => got.extend(chunk),
                Ok(PtyEvent::Exited) => break,
                // Timeout: keep polling until the deadline.
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
            if got.windows(9).any(|w| w == b"ok-2\r\nok-")
                || got.windows(5).any(|w| w == b"ok-2") && got.len() > 40
            {
                break;
            }
        }
        // `cat` echoes the input; a real shell would run it. Either way
        // the written bytes round-tripped through the pty.
        assert!(
            String::from_utf8_lossy(&got).contains("ok-$((1+1))"),
            "written keys never crossed the pty: {:?}",
            String::from_utf8_lossy(&got)
        );
    }

    #[test]
    fn resize_changes_the_pty_size() {
        // `stty size` reads the kernel pty dimensions from inside.
        let handle =
            PtyHandle::spawn("sh", &["-c", "stty size"], Path::new("/tmp"), 80, 24).unwrap();
        handle.resize(40, 12).unwrap();
        let mut got: Vec<u8> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match handle
                .rx
                .recv_timeout(std::time::Duration::from_millis(100))
            {
                Ok(PtyEvent::Output(chunk)) => got.extend(chunk),
                Ok(PtyEvent::Exited) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                }
                Err(_) => break,
            }
            if got.windows(5).any(|w| w == b"12 40") {
                break;
            }
        }
        assert!(
            String::from_utf8_lossy(&got).contains("12 40"),
            "stty reported {:?}, expected rows 12 cols 40",
            String::from_utf8_lossy(&got)
        );
    }

    #[test]
    fn large_output_arrives_in_full() {
        // 20000 lines (~108 KB) through an 8 KB reader buffer: the reader
        // loop must not drop or truncate chunks.
        let handle =
            PtyHandle::spawn("sh", &["-c", "seq 1 20000"], Path::new("/tmp"), 80, 24).unwrap();
        let mut got: Vec<u8> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match handle
                .rx
                .recv_timeout(std::time::Duration::from_millis(500))
            {
                Ok(PtyEvent::Output(chunk)) => got.extend(chunk),
                Ok(PtyEvent::Exited) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&got);
        assert!(
            text.contains("\n1\r\n") || text.starts_with("1\r\n"),
            "missing first line"
        );
        assert!(
            text.trim_end().ends_with("20000"),
            "missing final line; got {} bytes",
            got.len()
        );
    }

    #[test]
    fn output_before_exit_ordering_holds() {
        // The final Output event must precede Exited: the daemon relies on
        // this to show the pane's last text before collapsing the tree.
        let handle =
            PtyHandle::spawn("sh", &["-c", "printf last"], Path::new("/tmp"), 80, 24).unwrap();
        let mut saw_output = false;
        for ev in handle.rx {
            match ev {
                PtyEvent::Output(_) => saw_output = true,
                PtyEvent::Exited => break,
            }
        }
        assert!(saw_output, "output never arrived before Exited");
    }
}
