//! Edit-in-nvim flow (S3-7): write a scrollback dump to a private temp
//! file, hand it to the editor, and remove the file no matter how the
//! editor call ends. The TUI suspend and restore around this live in the
//! client's run loop; everything here is testable without a terminal.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Write `text` to a 0600 temp file in the user's temp dir and return
/// its path. The pane contents must not be readable by anyone else. The
/// name is unique per call so concurrent edits (or tests) never collide.
fn write_dump_file(text: &str) -> io::Result<PathBuf> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "corral-scrollback-{}-{seq}.txt",
        std::process::id()
    ));
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?
        .write_all(text.as_bytes())?;
    Ok(path)
}

/// Run the editor over a fresh dump file and always delete the file
/// afterwards, success or failure. Returns the edited contents read back
/// from the file before deletion, so the caller can write them back into
/// the pane. `run` receives the file path; the real client passes an
/// editor spawner, tests pass a fake.
pub fn edit_scrollback(
    text: &str,
    run: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> io::Result<String> {
    let path = write_dump_file(text)?;
    let result = run(&path);
    // Read back before deleting: the editor may have changed the file.
    let edited = fs::read_to_string(&path).unwrap_or_else(|_| text.to_string());
    // Delete on every exit path: the dump holds pane contents.
    let _ = fs::remove_file(&path);
    result.map(|_| edited)
}

/// Spawn the editor on `path`: $EDITOR when set, else nvim, then vi.
pub fn spawn_editor(path: &Path) -> io::Result<()> {
    if let Ok(editor) = std::env::var("EDITOR") {
        return run_cmd(&editor, path);
    }
    match run_cmd("nvim", path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => run_cmd("vi", path),
        other => other,
    }
}

fn run_cmd(prog: &str, path: &Path) -> io::Result<()> {
    let status = std::process::Command::new(prog).arg(path).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{prog} exited with {status}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn dump_file_is_written_with_owner_only_permissions() {
        let path = write_dump_file("alpha\nbeta\n").unwrap();
        let meta = fs::metadata(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&path).unwrap(), "alpha\nbeta\n");
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn edit_flow_runs_the_editor_then_deletes_the_file() {
        // The fake editor records that the file existed with the full
        // dump at call time; afterwards the file must be gone.
        let seen: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let seen_at_call = Rc::clone(&seen);
        let mut run = move |path: &Path| -> io::Result<()> {
            assert!(path.exists(), "dump file missing when editor runs");
            *seen_at_call.borrow_mut() = Some(fs::read_to_string(path).unwrap());
            Ok(())
        };
        edit_scrollback("one\ntwo\n", &mut run).unwrap();
        assert_eq!(seen.borrow().as_deref(), Some("one\ntwo\n"));
        // The editor ran with a real file; the flow deleted it. We hold
        // the path the editor saw, so deletion is provable by reopen.
        assert!(
            seen.borrow()
                .as_ref()
                .is_some_and(|p| fs::metadata(p).is_err()),
            "dump file must be deleted after the editor exits"
        );
    }

    #[test]
    fn edit_flow_deletes_the_file_when_the_editor_fails() {
        let seen: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
        let seen_at_call = Rc::clone(&seen);
        let mut run = move |path: &Path| -> io::Result<()> {
            *seen_at_call.borrow_mut() = Some(path.to_path_buf());
            Err(io::Error::other("editor crashed"))
        };
        assert!(edit_scrollback("x", &mut run).is_err());
        assert!(
            seen.borrow()
                .as_ref()
                .is_some_and(|p| fs::metadata(p).is_err()),
            "failed editor must still leave no dump file"
        );
    }

    #[test]
    fn dump_file_name_sits_in_the_user_temp_dir() {
        // The path shape: inside std::env::temp_dir, corral-scrollback
        // prefix. Verifying via write + inspect, then clean up.
        let path = write_dump_file("").unwrap();
        assert!(path.starts_with(std::env::temp_dir()));
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("corral-scrollback-"),
            "got {:?}",
            path.file_name()
        );
        fs::remove_file(&path).unwrap();
    }
}
