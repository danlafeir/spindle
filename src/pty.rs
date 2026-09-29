use std::io::{self, Read, Write};

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
    Ok(Pane { master: pair.master, child, reader, writer })
}

/// Copies until the reader side ends. Linux reports a hung-up PTY master as EIO rather than
/// EOF, so any read error is treated as end of stream; write errors are returned.
pub fn pump(reader: &mut impl Read, writer: &mut impl Write) -> io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => return Ok(()),
            Ok(n) => n,
        };
        writer.write_all(&buf[..n])?;
        writer.flush()?;
    }
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
        let mut pane = spawn(cmd, PtySize { rows: 24, cols: 80, ..Default::default() }).unwrap();

        let (tx, rx) = mpsc::channel();
        let mut reader = pane.reader;
        thread::spawn(move || {
            let mut out = Vec::new();
            pump(&mut reader, &mut out).unwrap();
            tx.send(out).unwrap();
        });
        let out = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reader never saw EOF after the child exited");

        assert!(String::from_utf8_lossy(&out).contains("hi"));
        assert!(pane.child.wait().unwrap().success());
    }
}
