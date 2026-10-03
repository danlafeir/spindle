use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use tui_term::widget::PseudoTerminal;

const SIDEBAR_WIDTH: u16 = 26;

pub struct Areas {
    pub sidebar: Rect,
    pub pane: Rect,
    pub pane_inner: Rect,
}

/// The single source of truth for geometry: drawing, the PTY size and the vt100 parser size
/// must all agree, or the shell wraps lines at the wrong column.
pub fn layout(area: Rect) -> Areas {
    let [sidebar, pane] =
        Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(1)]).areas(area);
    Areas {
        sidebar,
        pane,
        pane_inner: Block::bordered().inner(pane),
    }
}

pub fn draw(frame: &mut Frame, screen: &vt100::Screen) {
    let areas = layout(frame.area());

    let sidebar = Block::bordered().title(" spindle ");
    let inner = sidebar.inner(areas.sidebar);
    frame.render_widget(sidebar, areas.sidebar);

    let [menu, rule, body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);
    frame.render_widget(menu_line(), menu);
    frame.render_widget(Block::new().borders(Borders::TOP).dark_gray(), rule);
    frame.render_widget(Paragraph::new(placeholder_lists()), body);

    frame.render_widget(
        PseudoTerminal::new(screen).block(Block::bordered()),
        areas.pane,
    );
}

fn menu_line() -> Line<'static> {
    let key = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::raw("^b ").dark_gray(),
        Span::styled("n", key),
        Span::raw(" new "),
        Span::styled("s", key),
        Span::raw(" split "),
        Span::styled("q", key),
        Span::raw(" quit"),
    ])
}

// Static stand-ins until workspaces and agent detection exist.
fn placeholder_lists() -> Vec<Line<'static>> {
    let header = |s: &'static str| Line::from(s).bold().dark_gray();
    let agent = |dot: &'static str, name: &'static str, state: &'static str, color: Color| {
        Line::from(vec![
            Span::styled(dot, Style::new().fg(color)),
            Span::raw(format!(" {name:<9}")),
            Span::styled(state, Style::new().fg(color)),
        ])
    };
    vec![
        header("WORKSPACES"),
        Line::from("▸ spindle").bold(),
        Line::from("  dotfiles"),
        Line::from(""),
        header("AGENTS"),
        agent("●", "claude", "working", Color::Green),
        agent("◐", "codex", "blocked", Color::Yellow),
        agent("○", "shell", "idle", Color::DarkGray),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty;
    use portable_pty::{CommandBuilder, PtySize};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::io::Read;

    fn row(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[test]
    fn renders_menu_atop_sidebar_and_shell_output_in_pane() {
        let area = Rect::new(0, 0, 80, 24);
        let inner = layout(area).pane_inner;

        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", "printf 'hello from the pane'"]);
        let size = PtySize {
            rows: inner.height,
            cols: inner.width,
            ..Default::default()
        };
        let mut pane = pty::spawn(cmd, size).unwrap();
        let mut out = Vec::new();
        let _ = pane.reader.read_to_end(&mut out);
        pane.child.wait().unwrap();

        let mut parser = vt100::Parser::new(inner.height, inner.width, 0);
        parser.process(&out);

        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal.draw(|f| draw(f, parser.screen())).unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..area.height).map(|y| row(buf, y)).collect();
        println!("{}", rows.join("\n"));

        let sidebar = |y: usize| {
            rows[y][..]
                .chars()
                .take(SIDEBAR_WIDTH as usize)
                .collect::<String>()
        };
        assert!(sidebar(0).contains("spindle"));
        assert!(
            sidebar(1).contains("^b n new s split q quit"),
            "menu must be the first sidebar row"
        );
        assert!(sidebar(3).contains("WORKSPACES"));
        assert!(rows.iter().position(|r| r.contains("AGENTS")).unwrap() > 3);

        let pane_row: String = rows[inner.y as usize]
            .chars()
            .skip(inner.x as usize)
            .collect();
        assert!(pane_row.starts_with("hello from the pane"));
    }
}
