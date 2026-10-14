//! Child process helpers shared by every ffmpeg/ffprobe/mkvmerge caller.
use std::io;
use std::time::Duration;
use tokio::process::{Child, Command};

/// `Command::spawn`, retried briefly on ETXTBSY: a concurrent fork can still hold a just-written binary open.
pub async fn spawn(cmd: &mut Command) -> io::Result<Child> {
    for _ in 0..5 {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(26) => {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            r => return r,
        }
    }
    cmd.spawn()
}
