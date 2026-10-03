use std::io::{Read, Write};

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub struct Pane {
    pub master: Box<dyn MasterPty + Send>,
    pub child: Box<dyn Child + Send + Sync>,
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
}

pub fn spawn(cmd: CommandBuilder, size: PtySize) -> Result<Pane> {
    let pair = native_pty_system().openpty(size)?;
    let child = pair.slave.spawn_command(cmd)?;
    // If the parent keeps the slave open, the master never sees EOF after the child exits.
    drop(pair.slave);
    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    Ok(Pane {
        master: pair.master,
        child,
        reader,
        writer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn output_passes_through_and_ends_when_child_exits() {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", "echo hi"]);
        let mut pane = spawn(
            cmd,
            PtySize {
                rows: 24,
                cols: 80,
                ..Default::default()
            },
        )
        .unwrap();

        let (tx, rx) = mpsc::channel();
        let mut reader = pane.reader;
        thread::spawn(move || {
            let mut out = Vec::new();
            // A hung-up master reads as EOF on macOS but EIO on Linux; either ends the stream.
            let _ = reader.read_to_end(&mut out);
            tx.send(out).unwrap();
        });
        let out = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reader never saw EOF after the child exited");

        assert!(String::from_utf8_lossy(&out).contains("hi"));
        assert!(pane.child.wait().unwrap().success());
    }
}
