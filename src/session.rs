use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread;

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use ratatui::layout::{Position, Rect};

use crate::pty;
use crate::ui;
use crate::workspace::{Axis, Direction, PaneId, Workspace};

pub enum Event {
    Output(PaneId, Vec<u8>),
    Exited(PaneId),
    Resize,
    Keys(Vec<u8>),
}

struct Pane {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    parser: vt100::Parser,
    input: Sender<Vec<u8>>,
}

fn pty_size(inner: Rect) -> PtySize {
    PtySize {
        rows: inner.height,
        cols: inner.width,
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
        } = pty::spawn(cmd, pty_size(inner))?;
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
                parser: vt100::Parser::new(inner.height, inner.width, 0),
                input,
            },
        );
        Ok(id)
    }

    pub fn new_workspace(&mut self) -> Result<()> {
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
                if pane.parser.screen().size() != (inner.height, inner.width) {
                    pane.parser.screen_mut().set_size(inner.height, inner.width);
                    pane.master.resize(pty_size(inner))?;
                }
            }
        }
        Ok(())
    }

    pub fn select_workspace(&mut self, index: usize) {
        if index < self.workspaces.len() {
            self.active = index;
        }
    }

    pub fn focus_next(&mut self) {
        self.workspaces[self.active].focus_next();
    }

    pub fn focus_toward(&mut self, dir: Direction) {
        self.workspaces[self.active].focus_toward(dir, self.area);
    }

    /// Focuses the pane under a click; returns false if the click missed every pane.
    pub fn focus_at(&mut self, col: u16, row: u16) -> bool {
        let ws = &mut self.workspaces[self.active];
        let hit = ws
            .rects(self.area)
            .into_iter()
            .find(|(_, r)| r.contains(Position::new(col, row)));
        match hit {
            Some((id, _)) => {
                ws.set_focus(id);
                true
            }
            None => false,
        }
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
}
