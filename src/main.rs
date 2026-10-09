mod git;
mod input;
mod pty;
mod review;
mod ui;

use std::io::{self, IsTerminal, Read, Write};
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;

use anyhow::{Result, bail};
use crossterm::terminal;
use portable_pty::{CommandBuilder, PtySize};
use ratatui::layout::Rect;
use signal_hook::consts::SIGWINCH;
use signal_hook::iterator::Signals;

enum Event {
    Output(Vec<u8>),
    Exited,
    Resize,
    Keys(Vec<u8>),
}

struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        ratatui::restore();
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

fn pty_size(area: Rect) -> PtySize {
    PtySize {
        rows: area.height,
        cols: area.width,
        ..Default::default()
    }
}

fn run() -> Result<ExitCode> {
    if !io::stdin().is_terminal() {
        bail!("stdin is not a terminal");
    }

    let (cols, rows) = terminal::size()?;
    let inner = ui::layout(Rect::new(0, 0, cols, rows)).pane_inner;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut cmd = CommandBuilder::new(shell);
    cmd.cwd(std::env::current_dir()?);
    // vt100 is the emulator now, so advertise what it understands rather than the outer terminal.
    cmd.env("TERM", "xterm-256color");
    let pty::Pane {
        master,
        mut child,
        mut reader,
        mut writer,
    } = pty::spawn(cmd, pty_size(inner))?;
    let mut parser = vt100::Parser::new(inner.height, inner.width, 0);

    let mut term = ratatui::try_init()?;
    let _restore = RestoreTerminal;
    let (tx, rx) = mpsc::channel();

    let output_tx = tx.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        // Linux reports a hung-up PTY master as EIO rather than EOF, so any read error means exit.
        while let Ok(n @ 1..) = reader.read(&mut buf) {
            if output_tx.send(Event::Output(buf[..n].to_vec())).is_err() {
                return;
            }
        }
        let _ = output_tx.send(Event::Exited);
    });

    let (shell_tx, shell_rx) = mpsc::channel::<Vec<u8>>();
    // Writes to the PTY block while the shell isn't reading, so they get their own thread
    // rather than stalling rendering. It ends when the main loop drops shell_tx.
    thread::spawn(move || {
        for bytes in shell_rx {
            if writer
                .write_all(&bytes)
                .and_then(|_| writer.flush())
                .is_err()
            {
                return;
            }
        }
    });

    let input_tx = tx.clone();
    // Blocks on stdin forever so it can't be joined; it ends with the process.
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 1024];
        while let Ok(n @ 1..) = stdin.read(&mut buf) {
            if input_tx.send(Event::Keys(buf[..n].to_vec())).is_err() {
                return;
            }
        }
    });

    let mut winch = Signals::new([SIGWINCH])?;
    // Waits on signals forever; like the stdin thread, it ends with the process.
    thread::spawn(move || {
        for _ in winch.forever() {
            if tx.send(Event::Resize).is_err() {
                return;
            }
        }
    });

    let launch_dir = std::env::current_dir()?;
    let mut pane_inner = inner;
    let mut view = ui::View::Shell;
    let mut input = input::Input::default();
    term.draw(|f| ui::draw(f, parser.screen(), &view))?;
    for event in &rx {
        match event {
            Event::Output(bytes) => parser.process(&bytes),
            Event::Resize => {
                term.autoresize()?;
                let size = term.size()?;
                pane_inner = ui::layout(Rect::new(0, 0, size.width, size.height)).pane_inner;
                parser
                    .screen_mut()
                    .set_size(pane_inner.height, pane_inner.width);
                master.resize(pty_size(pane_inner))?;
            }
            Event::Exited => break,
            Event::Keys(bytes) => {
                for action in input.feed(&bytes) {
                    match action {
                        input::Action::Bytes(bytes) => match &mut view {
                            ui::View::Shell => {
                                let _ = shell_tx.send(bytes);
                            }
                            ui::View::Review(review) => {
                                let page = ui::review_page(pane_inner);
                                let closed = review::parse_keys(&bytes).into_iter().any(|key| {
                                    matches!(review.handle(key, page), review::Outcome::Close)
                                });
                                if closed {
                                    view = ui::View::Shell;
                                }
                            }
                            ui::View::Unavailable(_) => {
                                if review::parse_keys(&bytes).contains(&review::Key::Close) {
                                    view = ui::View::Shell;
                                }
                            }
                        },
                        input::Action::Mouse(_) => {}
                        input::Action::ToggleReview => {
                            view = match view {
                                ui::View::Shell => match review::Review::open(&launch_dir) {
                                    Ok(review) => ui::View::Review(Box::new(review)),
                                    Err(e) => ui::View::Unavailable(format!("{e:#}")),
                                },
                                _ => ui::View::Shell,
                            };
                        }
                        input::Action::Quit => {
                            child.kill()?;
                            child.wait()?;
                            return Ok(ExitCode::SUCCESS);
                        }
                    }
                }
            }
        }
        term.draw(|f| ui::draw(f, parser.screen(), &view))?;
    }

    let status = child.wait()?;
    Ok(ExitCode::from(status.exit_code().min(255) as u8))
}
