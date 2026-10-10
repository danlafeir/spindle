use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread;

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use ratatui::layout::{Position, Rect};

use crate::input::{Mouse, MouseKind};
use crate::pty;
use crate::ui;
use crate::workspace::{Axis, Direction, PaneId, Workspace};

pub const WHEEL_LINES: usize = 3;

pub enum Event {
    Output(PaneId, Vec<u8>),
    Exited(PaneId),
    Resize,
    Keys(Vec<u8>),
    Review(crate::review::Update),
}

struct Pane {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    parser: vt100::Parser,
    input: Sender<Vec<u8>>,
}

/// vt100 panics on a 0-row, 0-column or 1x1 screen, which a squeezed terminal can give a pane,
/// so the shell is told it has at least 2x2 and whatever doesn't fit is clipped when drawn.
const MIN_SCREEN: u16 = 2;

/// The (rows, cols) a pane's emulator and PTY use for a drawn area of `inner`.
fn screen_size(inner: Rect) -> (u16, u16) {
    (inner.height.max(MIN_SCREEN), inner.width.max(MIN_SCREEN))
}

fn pty_size((rows, cols): (u16, u16)) -> PtySize {
    PtySize {
        rows,
        cols,
        ..Default::default()
    }
}

/// Every running shell, grouped into workspaces. All workspaces share the pane area, so
/// switching between them is only a matter of which one is drawn.
pub struct Session {
    workspaces: Vec<Workspace>,
    active: usize,
    panes: HashMap<PaneId, Pane>,
    next_id: PaneId,
    shell: String,
    cwd: PathBuf,
    events: Sender<Event>,
    area: Rect,
    /// The pane a mouse button went down in, with the last report sent there. Drags and the
    /// release follow it there even if the pointer leaves, so the app never sees a button that
    /// is stuck down.
    mouse_grab: Option<(PaneId, Mouse)>,
}

impl Session {
    /// Starts with one workspace holding one shell, filling `area`.
    pub fn new(shell: String, cwd: PathBuf, area: Rect, events: Sender<Event>) -> Result<Session> {
        let mut session = Session {
            workspaces: Vec::new(),
            active: 0,
            panes: HashMap::new(),
            next_id: 0,
            shell,
            cwd,
            events,
            area,
            mouse_grab: None,
        };
        session.new_workspace()?;
        Ok(session)
    }

    fn spawn(&mut self, inner: Rect) -> Result<PaneId> {
        let id = self.next_id;
        let mut cmd = CommandBuilder::new(&self.shell);
        cmd.cwd(&self.cwd);
        // vt100 is the emulator now, so advertise what it understands rather than the outer terminal.
        cmd.env("TERM", "xterm-256color");
        let pty::Pane {
            master,
            child,
            mut reader,
            mut writer,
        } = pty::spawn(cmd, pty_size(screen_size(inner)))?;
        self.next_id += 1;

        let output = self.events.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            // Linux reports a hung-up PTY master as EIO rather than EOF, so any read error means exit.
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                if output.send(Event::Output(id, buf[..n].to_vec())).is_err() {
                    return;
                }
            }
            let _ = output.send(Event::Exited(id));
        });

        let (input, input_rx) = mpsc::channel::<Vec<u8>>();
        // Writes to the PTY block while the shell isn't reading, so they get their own thread
        // rather than stalling rendering. It ends when the pane is dropped with its sender.
        thread::spawn(move || {
            for bytes in input_rx {
                if writer
                    .write_all(&bytes)
                    .and_then(|_| writer.flush())
                    .is_err()
                {
                    return;
                }
            }
        });

        self.panes.insert(
            id,
            Pane {
                master,
                child,
                parser: {
                    let (rows, cols) = screen_size(inner);
                    vt100::Parser::new(rows, cols, 0)
                },
                input,
            },
        );
        Ok(id)
    }

    pub fn new_workspace(&mut self) -> Result<()> {
        self.release_mouse();
        let id = self.spawn(ui::pane_inner(self.area))?;
        self.workspaces.push(Workspace::new(id));
        self.active = self.workspaces.len() - 1;
        Ok(())
    }

    /// Returns false when the focused pane is too small to split.
    pub fn split(&mut self, axis: Axis) -> Result<bool> {
        if !self.workspaces[self.active].can_split(axis, self.area) {
            return Ok(false);
        }
        let id = self.spawn(ui::pane_inner(self.area))?;
        self.workspaces[self.active].split(axis, id);
        self.relayout()?;
        Ok(true)
    }

    /// Asks the focused shell to end; its pane goes away when [`Session::exited`] reports it.
    pub fn close_focused(&mut self) {
        let id = self.focused();
        // An error means the child is already gone, and its Exited event is on its way.
        let _ = self.panes.get_mut(&id).map(|p| p.child.kill());
    }

    /// Removes a pane whose shell has exited. Returns the exit code once no panes are left.
    pub fn exited(&mut self, id: PaneId) -> Result<Option<u32>> {
        let Some(mut pane) = self.panes.remove(&id) else {
            return Ok(None);
        };
        let status = pane.child.wait()?;
        let Some(ws) = self.workspaces.iter().position(|w| w.contains(id)) else {
            return Ok(None);
        };
        if self.workspaces[ws].remove(id) {
            self.workspaces.remove(ws);
            if self.workspaces.is_empty() {
                return Ok(Some(status.exit_code()));
            }
            if self.active >= ws && self.active > 0 {
                self.active -= 1;
            }
        }
        self.relayout()?;
        Ok(None)
    }

    pub fn kill_all(&mut self) -> Result<()> {
        for pane in self.panes.values_mut() {
            let _ = pane.child.kill();
        }
        for pane in self.panes.values_mut() {
            pane.child.wait()?;
        }
        Ok(())
    }

    pub fn is_visible(&self, id: PaneId) -> bool {
        self.workspaces[self.active].contains(id)
    }

    pub fn output(&mut self, id: PaneId, bytes: &[u8]) {
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.parser.process(bytes);
        }
    }

    pub fn send(&self, bytes: Vec<u8>) {
        if let Some(pane) = self.panes.get(&self.focused()) {
            let _ = pane.input.send(bytes);
        }
    }

    fn focused(&self) -> PaneId {
        self.workspaces[self.active].focus()
    }

    pub fn resize(&mut self, area: Rect) -> Result<()> {
        self.area = area;
        self.relayout()
    }

    /// Brings every pane's PTY and emulator to the size it's drawn at. Hidden workspaces are
    /// included so their shells already have the right size when switched to.
    fn relayout(&mut self) -> Result<()> {
        for ws in &self.workspaces {
            for (id, rect) in ws.rects(self.area) {
                let inner = ui::pane_inner(rect);
                let pane = self.panes.get_mut(&id).expect("every tree pane is live");
                let size = screen_size(inner);
                if pane.parser.screen().size() != size {
                    pane.parser.screen_mut().set_size(size.0, size.1);
                    pane.master.resize(pty_size(size))?;
                }
            }
        }
        Ok(())
    }

    pub fn select_workspace(&mut self, index: usize) {
        if index < self.workspaces.len() {
            self.release_mouse();
            self.active = index;
        }
    }

    pub fn grabbing_mouse(&self) -> bool {
        self.mouse_grab.is_some()
    }

    /// Ends a drag when spindle takes the mouse away from the panes, by sending the app the
    /// release it would otherwise never get.
    pub fn release_mouse(&mut self) {
        let Some((id, last)) = self.mouse_grab.take() else {
            return;
        };
        let rect = self
            .workspaces
            .iter()
            .flat_map(|ws| ws.rects(self.area))
            .find_map(|(pane, rect)| (pane == id).then_some(rect));
        if let Some(rect) = rect {
            let release = Mouse {
                code: last.code & !32,
                release: true,
                ..last
            };
            self.send_mouse(id, ui::pane_inner(rect), release);
        }
    }

    /// Sends `mouse` to the app in pane `id`, relative to its screen and clamped to it. False
    /// when the app didn't ask for this kind of report.
    fn send_mouse(&self, id: PaneId, inner: Rect, mouse: Mouse) -> bool {
        let Some(pane) = self.panes.get(&id) else {
            return false;
        };
        let col = mouse
            .col
            .saturating_sub(inner.x)
            .min(inner.width.saturating_sub(1));
        let row = mouse
            .row
            .saturating_sub(inner.y)
            .min(inner.height.saturating_sub(1));
        let screen = pane.parser.screen();
        let mode = screen.mouse_protocol_mode();
        match mouse.encode_for(col, row, mode, screen.mouse_protocol_encoding()) {
            Some(bytes) => {
                let _ = pane.input.send(bytes);
                true
            }
            None => false,
        }
    }

    /// Handles a mouse report over the panes: a press focuses its pane, and the report goes on
    /// to the app there if it asked for the mouse. Returns false when the report isn't over a
    /// pane and no drag is under way, leaving it for the caller.
    pub fn mouse(&mut self, mouse: Mouse) -> bool {
        let pos = Position::new(mouse.col, mouse.row);
        let rects = self.workspaces[self.active].rects(self.area);
        let under = rects
            .iter()
            .find(|(_, r)| r.contains(pos))
            .map(|(id, _)| *id);
        let target = self.mouse_grab.map(|(id, _)| id).or(under);
        let Some((id, rect)) = rects.into_iter().find(|(id, _)| Some(*id) == target) else {
            self.mouse_grab = None;
            return false;
        };
        let inner = ui::pane_inner(rect);
        let press = !mouse.release && !mouse.is_motion() && !mouse.is_wheel();
        if press {
            self.workspaces[self.active].set_focus(id);
        }
        // A press on the border only focuses; it isn't inside the app's screen.
        if self.mouse_grab.is_none() && !inner.contains(pos) {
            return true;
        }
        if press {
            self.mouse_grab = Some((id, mouse));
        } else if mouse.release && !mouse.is_wheel() {
            self.mouse_grab = None;
        } else if let Some((_, last)) = &mut self.mouse_grab
            && mouse.is_motion()
        {
            *last = mouse;
        }

        if self.send_mouse(id, inner, mouse) {
            return true;
        }
        let pane = &self.panes[&id];
        let screen = pane.parser.screen();
        let mode = screen.mouse_protocol_mode();
        // Like most terminals, scroll full-screen apps that don't take the mouse (less, man)
        // with arrow keys. At a shell prompt that would walk history, so it's left alone there.
        if mode == vt100::MouseProtocolMode::None && screen.alternate_screen() {
            let arrow = match (mouse.kind(), screen.application_cursor()) {
                (MouseKind::WheelUp, false) => "\x1b[A",
                (MouseKind::WheelDown, false) => "\x1b[B",
                (MouseKind::WheelUp, true) => "\x1bOA",
                (MouseKind::WheelDown, true) => "\x1bOB",
                _ => return true,
            };
            let _ = pane.input.send(arrow.repeat(WHEEL_LINES).into_bytes());
        }
        true
    }

    pub fn focus_next(&mut self) {
        self.workspaces[self.active].focus_next();
    }

    pub fn focus_toward(&mut self, dir: Direction) {
        self.workspaces[self.active].focus_toward(dir, self.area);
    }

    pub fn workspace_count(&self) -> usize {
        self.workspaces.len()
    }

    pub fn active(&self) -> usize {
        self.active
    }

    /// The active workspace's panes as the UI draws them.
    pub fn visible(&self) -> Vec<ui::PaneView<'_>> {
        let ws = &self.workspaces[self.active];
        let many = ws.panes().len() > 1;
        ws.rects(self.area)
            .into_iter()
            .map(|(id, area)| ui::PaneView {
                area,
                screen: self.panes[&id].parser.screen(),
                focused: many && id == ws.focus(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    /// Runs `script` in the focused shell and feeds events back until that pane exits.
    fn exit_focused(session: &mut Session, rx: &Receiver<Event>, code: u8) -> Option<u32> {
        let id = session.focused();
        session.send(format!("exit {code}\n").into_bytes());
        loop {
            match rx
                .recv_timeout(Duration::from_secs(5))
                .expect("shell never exited")
            {
                Event::Output(id, bytes) => session.output(id, &bytes),
                Event::Exited(gone) => {
                    let done = session.exited(gone).unwrap();
                    if gone == id {
                        return done;
                    }
                }
                _ => {}
            }
        }
    }

    #[test]
    fn panes_and_workspaces_close_as_their_shells_exit() {
        let (tx, rx) = mpsc::channel();
        let area = Rect::new(26, 0, 100, 40);
        let mut session = Session::new("/bin/sh".into(), std::env::temp_dir(), area, tx).unwrap();
        assert!(session.split(Axis::SideBySide).unwrap());
        let widths: Vec<u16> = session.visible().iter().map(|p| p.area.width).collect();
        assert_eq!(widths, vec![50, 50]);
        assert_eq!(
            session.visible()[1].screen.size(),
            (38, 48),
            "PTY sized to its half"
        );

        assert_eq!(exit_focused(&mut session, &rx, 3), None);
        assert_eq!(session.visible().len(), 1);
        assert_eq!(
            session.visible()[0].screen.size(),
            (38, 98),
            "survivor regrows"
        );

        session.new_workspace().unwrap();
        assert_eq!((session.workspace_count(), session.active()), (2, 1));
        assert_eq!(exit_focused(&mut session, &rx, 7), None);
        assert_eq!((session.workspace_count(), session.active()), (1, 0));

        assert_eq!(
            exit_focused(&mut session, &rx, 5),
            Some(5),
            "last shell's code"
        );
    }

    #[test]
    fn closing_kills_the_focused_shell() {
        let (tx, rx) = mpsc::channel();
        let area = Rect::new(0, 0, 100, 40);
        let mut session = Session::new("/bin/sh".into(), std::env::temp_dir(), area, tx).unwrap();
        session.split(Axis::Stacked).unwrap();
        let doomed = session.focused();
        session.close_focused();
        loop {
            if let Event::Exited(id) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                assert_eq!(id, doomed);
                assert_eq!(session.exited(id).unwrap(), None);
                break;
            }
        }
        assert_eq!(session.visible().len(), 1);
        session.kill_all().unwrap();
    }

    #[test]
    fn mouse_reaches_apps_that_ask_for_it_relative_to_their_pane() {
        let (tx, _rx) = mpsc::channel();
        let area = Rect::new(26, 0, 80, 24);
        let mut session = Session::new("/bin/sh".into(), std::env::temp_dir(), area, tx).unwrap();
        let id = session.focused();
        // Stand in for the PTY writer so the test sees exactly what the app would read.
        let (typed_tx, typed) = mpsc::channel();
        session.panes.get_mut(&id).unwrap().input = typed_tx;
        let sent = || typed.try_iter().flatten().collect::<Vec<u8>>();

        let click = Mouse {
            code: 0,
            col: 36,
            row: 5,
            release: false,
        };
        let release = Mouse {
            release: true,
            ..click
        };
        let wheel_down = Mouse { code: 65, ..click };

        assert!(session.mouse(click));
        assert!(session.grabbing_mouse());
        session.mouse(release);
        assert!(!session.grabbing_mouse(), "release ends the grab");
        session.mouse(wheel_down);
        assert_eq!(
            sent(),
            b"",
            "a shell prompt gets neither clicks nor wheel arrows"
        );

        // less and man use the alternate screen without asking for the mouse.
        session.output(id, b"\x1b[?1049h");
        session.mouse(wheel_down);
        assert_eq!(sent(), b"\x1b[B\x1b[B\x1b[B");

        session.output(id, b"\x1b[?1000h\x1b[?1006h");
        session.mouse(click);
        // A release dragged out over the sidebar still goes to the pane, clamped to its edge.
        session.mouse(Mouse { col: 0, ..release });
        assert_eq!(
            sent(),
            b"\x1b[<0;10;5M\x1b[<0;1;5m",
            "the pane's screen starts at column 27, so column 36 is the app's 10th"
        );

        // A press on the border only focuses.
        session.mouse(Mouse { col: 26, ..click });
        assert_eq!(sent(), b"");
        assert!(!session.grabbing_mouse());
        session.kill_all().unwrap();
    }

    #[test]
    fn squeezed_panes_survive() {
        let (tx, _rx) = mpsc::channel();
        // Wide enough for the sidebar only: the pane's inside is 0 columns wide.
        let mut session = Session::new(
            "/bin/sh".into(),
            std::env::temp_dir(),
            Rect::new(26, 0, 2, 40),
            tx,
        )
        .unwrap();
        session.output(session.focused(), b"hello\r\nworld");
        for area in [
            Rect::new(26, 0, 0, 0),
            Rect::new(26, 0, 3, 3),
            Rect::new(26, 0, 80, 2),
            Rect::new(26, 0, 80, 24),
        ] {
            session.resize(area).unwrap();
            session.output(session.focused(), b"more\r\n");
        }
        assert_eq!(session.visible()[0].screen.size(), (22, 78));
        session.kill_all().unwrap();
    }

    #[test]
    fn splits_stop_when_panes_get_too_small() {
        let (tx, _rx) = mpsc::channel();
        let area = Rect::new(0, 0, 38, 40);
        let mut session = Session::new("/bin/sh".into(), std::env::temp_dir(), area, tx).unwrap();
        assert!(session.split(Axis::SideBySide).unwrap());
        assert!(
            !session.split(Axis::SideBySide).unwrap(),
            "19 columns can't halve again"
        );
        assert_eq!(session.visible().len(), 2);
        session.kill_all().unwrap();
    }

    #[test]
    fn a_drag_spindle_takes_the_mouse_from_is_released() {
        let (tx, _rx) = mpsc::channel();
        let area = Rect::new(26, 0, 80, 24);
        let mut session = Session::new("/bin/sh".into(), std::env::temp_dir(), area, tx).unwrap();
        let id = session.focused();
        let (typed_tx, typed) = mpsc::channel();
        session.panes.get_mut(&id).unwrap().input = typed_tx;
        let sent = || typed.try_iter().flatten().collect::<Vec<u8>>();
        session.output(id, b"\x1b[?1002h\x1b[?1006h");

        let press = Mouse {
            code: 0,
            col: 36,
            row: 5,
            release: false,
        };
        session.mouse(press);
        session.mouse(Mouse {
            code: 32,
            col: 40,
            ..press
        });
        assert_eq!(sent(), b"\x1b[<0;10;5M\x1b[<32;14;5M");

        session.release_mouse();
        assert_eq!(sent(), b"\x1b[<0;14;5m", "released where the drag got to");
        assert!(!session.grabbing_mouse());
        session.release_mouse();
        assert_eq!(sent(), b"", "nothing held, nothing sent");

        session.mouse(press);
        session.new_workspace().unwrap();
        assert_eq!(
            sent(),
            b"\x1b[<0;10;5M\x1b[<0;10;5m",
            "switching away releases it in the hidden pane"
        );
        session.kill_all().unwrap();
    }
}
