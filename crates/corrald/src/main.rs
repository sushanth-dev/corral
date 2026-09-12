mod protocol;
mod pty;
mod server;

use std::os::unix::net::UnixListener;

fn main() -> anyhow::Result<()> {
    let path = server::socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    server::Daemon::serve(listener)
}
