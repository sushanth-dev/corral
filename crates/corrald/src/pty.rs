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
}
