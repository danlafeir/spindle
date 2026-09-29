mod pty;

use std::io::{self, IsTerminal};
use std::process::ExitCode;
use std::thread;

use anyhow::{Result, bail};
use crossterm::terminal;
use portable_pty::{CommandBuilder, PtySize};
use signal_hook::consts::SIGWINCH;
use signal_hook::iterator::Signals;

struct RawMode;

impl RawMode {
    fn enable() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(RawMode)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("spindle: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    if !io::stdin().is_terminal() {
        bail!("stdin is not a terminal");
    }

    let (cols, rows) = terminal::size()?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut cmd = CommandBuilder::new(shell);
    cmd.cwd(std::env::current_dir()?);
    let pty::Pane { master, mut child, mut reader, mut writer } =
        pty::spawn(cmd, PtySize { rows, cols, ..Default::default() })?;

    let _raw = RawMode::enable()?;

    // Blocks on stdin forever so it can't be joined; it ends with the process once the shell exits.
    thread::spawn(move || pty::pump(&mut io::stdin().lock(), &mut writer));

    let mut winch = Signals::new([SIGWINCH])?;
    // Waits on signals forever; like the stdin thread, it ends with the process.
    thread::spawn(move || {
        for _ in winch.forever() {
            if let Ok((cols, rows)) = terminal::size() {
                let _ = master.resize(PtySize { rows, cols, ..Default::default() });
            }
        }
    });

    pty::pump(&mut reader, &mut io::stdout().lock())?;
    let status = child.wait()?;
    Ok(ExitCode::from(status.exit_code().min(255) as u8))
}
