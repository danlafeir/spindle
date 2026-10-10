mod config;
mod git;
mod input;
mod pty;
mod review;
mod session;
mod ui;
mod workspace;

use std::collections::VecDeque;
use std::io::{self, IsTerminal, Read, Write};
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use anyhow::{Result, bail};
use crossterm::terminal;
use ratatui::layout::Rect;
use signal_hook::consts::SIGWINCH;
use signal_hook::iterator::Signals;

use session::{Event, Session};

// Click and drag reports (1000, 1002) in SGR encoding (1006), which has no 223-column limit.
// Any-motion tracking (1003) is left off so idle mouse movement doesn't wake the loop.
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &[u8] = b"\x1b[?1006l\x1b[?1002l\x1b[?1000l";

const FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(16);

struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        // ratatui::restore doesn't know about mouse capture; left on, the shell spindle exits
        // to would be flooded with mouse reports on every click.
        let _ = io::stdout()
            .write_all(MOUSE_OFF)
            .and_then(|_| io::stdout().flush());
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

fn run() -> Result<ExitCode> {
    if !io::stdin().is_terminal() {
        bail!("stdin is not a terminal");
    }
    // Before any shell starts, so a bad config exits cleanly.
    let config = match config::path() {
        Some(path) => config::load(&path)?,
        None => config::Config::default(),
    };

    let launch_dir = std::env::current_dir()?;
    let sidebar_dir = review::clean(
        &launch_dir
            .file_name()
            .map_or_else(|| "/".into(), |n| n.to_string_lossy()),
    );
    let (cols, rows) = terminal::size()?;
    let (tx, rx) = mpsc::channel();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut session = Session::new(
        shell,
        launch_dir.clone(),
        ui::layout(Rect::new(0, 0, cols, rows)).pane,
        tx.clone(),
    )?;

    let mut term = ratatui::try_init()?;
    let _restore = RestoreTerminal;
    io::stdout().write_all(MOUSE_ON)?;
    io::stdout().flush()?;

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

    let review_tx = tx.clone();
    let mut winch = Signals::new([SIGWINCH])?;
    // Waits on signals forever; like the stdin thread, it ends with the process.
    thread::spawn(move || {
        for _ in winch.forever() {
            if tx.send(Event::Resize).is_err() {
                return;
            }
        }
    });

    let mut view = ui::View::Shell;
    let mut input = input::Input::new(config.prefix);
    let mut flush_at: Option<Instant> = None;
    let sidebar = |session: &Session| ui::Sidebar {
        workspaces: session.workspace_count(),
        active: session.active(),
        dir: sidebar_dir.clone(),
        prefix: config.prefix,
    };
    let mut dirty = true;
    loop {
        if dirty {
            term.draw(|f| ui::draw(f, &sidebar(&session), &session.visible(), &view))?;
            dirty = false;
        }
        let mut event = match flush_at {
            Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    unreachable!("the session holds a sender for the whole loop")
                }
            },
            None => Some(
                rx.recv()
                    .expect("the session holds a sender for the whole loop"),
            ),
        };
        // Everything already queued is handled before the next frame, so a flood of output
        // costs one redraw per frame rather than one per read. The budget keeps keystrokes
        // waiting behind that flood from going unseen for long.
        let batch_started = Instant::now();
        loop {
            // Output from a hidden workspace changes nothing on screen.
            dirty |= !matches!(&event, Some(Event::Output(id, _)) if !session.is_visible(*id));
            let keys = match event {
                None => input.flush(),
                Some(Event::Keys(bytes)) => input.feed(&bytes),
                Some(Event::Output(id, bytes)) => {
                    session.output(id, &bytes);
                    Vec::new()
                }
                Some(Event::Resize) => {
                    term.autoresize()?;
                    let size = term.size()?;
                    session.resize(ui::layout(Rect::new(0, 0, size.width, size.height)).pane)?;
                    Vec::new()
                }
                Some(Event::Review(update)) => {
                    if let ui::View::Review(review) = &mut view {
                        review.apply(update);
                    }
                    Vec::new()
                }
                Some(Event::Exited(id)) => {
                    if let Some(code) = session.exited(id)? {
                        return Ok(ExitCode::from(code.min(255) as u8));
                    }
                    Vec::new()
                }
            };
            // Measured from when bytes were first held, so a stream of shell output can't keep
            // pushing the wait back.
            flush_at = match (input.pending(), flush_at) {
                (false, _) => None,
                (true, Some(at)) => Some(at),
                (true, None) => Some(Instant::now() + input::ESC_WAIT),
            };
            let mut actions = VecDeque::from(keys);
            while let Some(action) = actions.pop_front() {
                // Pane and workspace commands act on the shells, so they bring them back into view.
                let shows_panes = matches!(
                    action,
                    input::Action::NewWorkspace
                        | input::Action::Split(_)
                        | input::Action::ClosePane
                        | input::Action::FocusNext
                        | input::Action::Focus(_)
                        | input::Action::SelectWorkspace(_)
                );
                if shows_panes {
                    view = ui::View::Shell;
                }
                match action {
                    input::Action::Bytes(bytes) => match &mut view {
                        ui::View::Shell => session.send(bytes),
                        ui::View::Review(review) => {
                            let page = ui::review_page(ui::layout(term_area(&term)?).pane_inner);
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
                    input::Action::Mouse(mouse) => {
                        let hit = ui::hit(
                            term_area(&term)?,
                            &view,
                            &sidebar(&session),
                            mouse.col,
                            mouse.row,
                        );
                        let over_panes = matches!(hit, ui::Hit::Nothing);
                        if matches!(view, ui::View::Shell)
                            && (over_panes || session.grabbing_mouse())
                            && session.mouse(mouse)
                        {
                            continue;
                        }
                        match (hit, mouse.kind()) {
                            (ui::Hit::MenuKey(key), input::MouseKind::LeftPress) => {
                                actions.extend(input::command(key));
                            }
                            (ui::Hit::Workspace(i), input::MouseKind::LeftPress) => {
                                actions.push_back(input::Action::SelectWorkspace(i));
                            }
                            (ui::Hit::TreeRow(row), input::MouseKind::LeftPress) => {
                                if let ui::View::Review(review) = &mut view {
                                    review.click(row);
                                }
                            }
                            (
                                ui::Hit::TreeRow(_) | ui::Hit::ReviewPane,
                                wheel @ (input::MouseKind::WheelUp | input::MouseKind::WheelDown),
                            ) => {
                                if let ui::View::Review(review) = &mut view {
                                    let key = if wheel == input::MouseKind::WheelUp {
                                        review::Key::ScrollUp
                                    } else {
                                        review::Key::ScrollDown
                                    };
                                    review.handle(key, session::WHEEL_LINES);
                                }
                            }
                            _ => {}
                        }
                    }
                    input::Action::ToggleReview => {
                        view = match view {
                            ui::View::Shell => match review::Review::open(&launch_dir, {
                                let tx = review_tx.clone();
                                // Fails only once the loop has ended and nothing is left to show it.
                                move |update| {
                                    let _ = tx.send(Event::Review(update));
                                }
                            }) {
                                Ok(review) => ui::View::Review(Box::new(review)),
                                Err(e) => ui::View::Unavailable(format!("{e:#}")),
                            },
                            _ => ui::View::Shell,
                        };
                    }
                    input::Action::NewWorkspace => {
                        if let Err(e) = session.new_workspace() {
                            view = ui::View::Unavailable(format!("Can't start a shell: {e:#}"));
                        }
                    }
                    input::Action::Split(axis) => {
                        if let Err(e) = session.split(axis) {
                            view = ui::View::Unavailable(format!("Can't start a shell: {e:#}"));
                        }
                    }
                    input::Action::ClosePane => session.close_focused(),
                    input::Action::FocusNext => session.focus_next(),
                    input::Action::Focus(dir) => session.focus_toward(dir),
                    input::Action::SelectWorkspace(i) => session.select_workspace(i),
                    input::Action::Quit => {
                        session.kill_all()?;
                        return Ok(ExitCode::SUCCESS);
                    }
                }
            }
            if batch_started.elapsed() >= FRAME_BUDGET {
                break;
            }
            event = match rx.try_recv() {
                Ok(event) => Some(event),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    unreachable!("the session holds a sender for the whole loop")
                }
            };
        }
    }
}

fn term_area(term: &ratatui::DefaultTerminal) -> Result<Rect> {
    let size = term.size()?;
    Ok(Rect::new(0, 0, size.width, size.height))
}
