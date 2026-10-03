use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tui_term::widget::PseudoTerminal;

use crate::git::Change;
use crate::review::{Content, Review, Row, RowKind};

const SIDEBAR_WIDTH: u16 = 26;

pub enum View {
    Shell,
    Review(Box<Review>),
    Unavailable(String),
}

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

/// How far one scroll key moves the review content: half its visible height.
pub fn review_page(pane_inner: Rect) -> usize {
    (pane_inner.height.saturating_sub(1) / 2).max(1) as usize
}

pub fn draw(frame: &mut Frame, screen: &vt100::Screen, view: &View) {
    let areas = layout(frame.area());

    let sidebar = Block::bordered().title(" spindle ");
    let inner = sidebar.inner(areas.sidebar);
    frame.render_widget(sidebar, areas.sidebar);

    let [menu, rule, body] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(menu_lines()), menu);
    frame.render_widget(Block::new().borders(Borders::TOP).dark_gray(), rule);
    frame.render_widget(Paragraph::new(placeholder_lists()), body);

    match view {
        View::Shell => frame.render_widget(
            PseudoTerminal::new(screen).block(Block::bordered()),
            areas.pane,
        ),
        View::Review(review) => draw_review(frame, areas.pane, review),
        View::Unavailable(msg) => frame.render_widget(
            Paragraph::new(vec![
                Line::from(msg.as_str()),
                Line::from(""),
                Line::from("Press q or Esc to go back.").dark_gray(),
            ])
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" review ")),
            areas.pane,
        ),
    }
}

fn menu_lines() -> Vec<Line<'static>> {
    let key = |k: &'static str| {
        Span::styled(k, Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
    };
    vec![
        Line::from(vec![
            Span::raw("^b ").dark_gray(),
            key("n"),
            Span::raw(" new  "),
            key("s"),
            Span::raw(" split"),
        ]),
        Line::from(vec![
            Span::raw("   "),
            key("g"),
            Span::raw(" review "),
            key("q"),
            Span::raw(" quit"),
        ]),
    ]
}

fn draw_review(frame: &mut Frame, area: Rect, review: &Review) {
    let block = Block::bordered()
        .title(format!(" review: {} vs HEAD ", review.root_name()))
        .title_bottom(
            Line::from(" j/k move  h/l fold  J/K scroll  r refresh  q close ").dark_gray(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let tree_width = (inner.width * 35 / 100).clamp(20, 40).min(inner.width);
    let [tree, content] =
        Layout::horizontal([Constraint::Length(tree_width), Constraint::Min(0)]).areas(inner);

    // Draws only the visible window so large repos don't build every row each frame.
    let height = tree.height as usize;
    let offset = review.cursor().saturating_sub(height.saturating_sub(1));
    let rows: Vec<Line> = review
        .rows()
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, row)| tree_line(row, i == review.cursor()))
        .collect();
    frame.render_widget(Paragraph::new(rows), tree);

    let divider = Block::new()
        .borders(Borders::LEFT)
        .border_style(Style::new().dark_gray());
    let content_inner = divider.inner(content);
    frame.render_widget(divider, content);
    let [header, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(content_inner);
    let title = review
        .selected()
        .map_or_else(String::new, |r| r.path.display().to_string());
    frame.render_widget(Line::from(format!(" {title}")).bold(), header);
    frame.render_widget(content_paragraph(review, body.height as usize), body);
}

fn change_color(change: &Change) -> Color {
    match change {
        Change::Modified => Color::Yellow,
        Change::Added | Change::Untracked => Color::Green,
        Change::Deleted => Color::Red,
        Change::Renamed { .. } => Color::Cyan,
    }
}

fn tree_line(row: &Row, selected: bool) -> Line<'static> {
    let indent = Span::raw("  ".repeat(row.depth));
    let line = match &row.kind {
        RowKind::Dir { open, changed } => {
            let style = if *changed {
                Style::new().fg(Color::Yellow).bold()
            } else {
                Style::new().fg(Color::Blue)
            };
            let arrow = if *open { "▾ " } else { "▸ " };
            Line::from(vec![
                indent,
                Span::styled(arrow, style),
                Span::styled(format!("{}/", row.name()), style),
            ])
        }
        RowKind::File {
            change: Some(change),
        } => {
            let style = Style::new().fg(change_color(change));
            Line::from(vec![
                indent,
                Span::styled(format!("{} ", change.marker()), style.bold()),
                Span::styled(row.name(), style),
            ])
        }
        RowKind::File { change: None } => {
            Line::from(vec![indent, Span::raw("  "), Span::raw(row.name())])
        }
    };
    if selected { line.reversed() } else { line }
}

fn content_paragraph(review: &Review, height: usize) -> Paragraph<'_> {
    let visible = |lines: &[String]| -> std::ops::Range<usize> {
        let start = review.scroll().min(lines.len());
        start..(start + height).min(lines.len())
    };
    let lines: Vec<Line> = match review.content() {
        Content::Message(msg) => {
            return Paragraph::new(Line::from(format!(" {msg}")).dark_gray())
                .wrap(Wrap { trim: false });
        }
        Content::Diff(lines) => lines[visible(lines)]
            .iter()
            .map(|l| {
                let style = if l.starts_with("+++") || l.starts_with("---") {
                    Style::new().bold()
                } else if l.starts_with('+') {
                    Style::new().fg(Color::Green)
                } else if l.starts_with('-') {
                    Style::new().fg(Color::Red)
                } else if l.starts_with("@@") {
                    Style::new().fg(Color::Cyan)
                } else if l.starts_with("diff ") || l.starts_with("index ") {
                    Style::new().dark_gray()
                } else {
                    Style::new()
                };
                Line::styled(format!(" {l}"), style)
            })
            .collect(),
        Content::File(lines) => {
            let width = lines.len().to_string().len();
            let range = visible(lines);
            lines[range.clone()]
                .iter()
                .zip(range)
                .map(|(l, i)| {
                    Line::from(vec![
                        Span::raw(format!(" {:>width$} ", i + 1)).dark_gray(),
                        Span::raw(l.as_str()),
                    ])
                })
                .collect()
        }
    };
    Paragraph::new(lines)
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
        terminal
            .draw(|f| draw(f, parser.screen(), &View::Shell))
            .unwrap();
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
            sidebar(1).contains("^b n new  s split"),
            "menu must be the first sidebar row"
        );
        assert!(sidebar(2).contains("g review q quit"));
        assert!(sidebar(4).contains("WORKSPACES"));
        assert!(rows.iter().position(|r| r.contains("AGENTS")).unwrap() > 4);

        let pane_row: String = rows[inner.y as usize]
            .chars()
            .skip(inner.x as usize)
            .collect();
        assert!(pane_row.starts_with("hello from the pane"));
    }

    #[test]
    fn renders_review_view_over_the_pane() {
        let tmp = crate::git::tests::fixture();
        let review = Review::open(tmp.path()).unwrap();
        let area = Rect::new(0, 0, 100, 20);
        let parser = vt100::Parser::new(1, 1, 0);

        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|f| draw(f, parser.screen(), &View::Review(Box::new(review))))
            .unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..area.height).map(|y| row(buf, y)).collect();
        println!("{}", rows.join("\n"));

        let text = rows.join("\n");
        assert!(text.contains("review: "));
        assert!(text.contains("▾ src/"));
        assert!(text.contains("M dir file.txt"));
        assert!(text.contains("? new.rs"));
        assert!(text.contains("R new_name.txt"));
        assert!(text.contains("D staged_gone.txt"));
        assert!(text.contains("+spaced and changed"));
        assert!(text.contains("q close"));
        assert!(!text.contains('\x1b'));
    }

    #[test]
    fn file_contents_never_reach_the_buffer_as_escapes() {
        let tmp = crate::git::tests::fixture();
        crate::git::tests::write(tmp.path(), "keep.txt", "evil \x1b[31m\tline\n");
        let mut review = Review::open(tmp.path()).unwrap();
        while review.selected().unwrap().path != std::path::Path::new("keep.txt") {
            review.handle(crate::review::Key::Down, 1);
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        let parser = vt100::Parser::new(1, 1, 0);
        terminal
            .draw(|f| draw(f, parser.screen(), &View::Review(Box::new(review))))
            .unwrap();
        let buf = terminal.backend().buffer();
        let text: String = (0..20).map(|y| row(buf, y)).collect();
        assert!(text.contains("evil"));
        assert!(!text.contains('\x1b'));
    }

    #[test]
    fn hostile_file_names_never_reach_the_buffer_as_escapes() {
        let tmp = tempfile::tempdir().unwrap();
        crate::git::tests::git(tmp.path(), &["init", "-q"]);
        crate::git::tests::write(tmp.path(), "evil\x1b]0;pwned\x07.txt", "x\n");
        let review = Review::open(tmp.path()).unwrap();
        assert!(review.selected().unwrap().name().contains("pwned"));

        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        let parser = vt100::Parser::new(1, 1, 0);
        terminal
            .draw(|f| draw(f, parser.screen(), &View::Review(Box::new(review))))
            .unwrap();
        let buf = terminal.backend().buffer();
        let text: String = (0..10).map(|y| row(buf, y)).collect();
        assert!(text.contains("pwned"));
        assert!(!text.contains('\x1b') && !text.contains('\x07'), "{text:?}");
    }
}
