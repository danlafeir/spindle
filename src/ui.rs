use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use tui_term::widget::PseudoTerminal;

use crate::config::Prefix;
use crate::git::Change;
use crate::input::Action;
use crate::review::{Content, Review, Row, RowKind};

const SIDEBAR_WIDTH: u16 = 26;
const MENU_HEIGHT: u16 = 4;

pub enum View {
    Shell,
    Review(Box<Review>),
    Unavailable(String),
}

/// Whether the help overlay is up. `Closing` is already off screen but still swallows mouse
/// reports until the release of the click that closed it, so that release can't reach a pane.
#[derive(PartialEq)]
pub enum Help {
    Hidden,
    Shown,
    Closing,
}

impl Help {
    /// While help is up any key or click closes it and is dropped rather than reaching what's
    /// beneath; a command closes it and still runs. True when `action` should be dropped.
    pub fn swallows(&mut self, action: &Action) -> bool {
        match (&*self, action) {
            (Help::Hidden, _) => false,
            (Help::Closing, Action::Mouse(mouse)) => {
                if mouse.release {
                    *self = Help::Hidden;
                }
                true
            }
            (Help::Shown, Action::Mouse(mouse)) => {
                if !mouse.release && !mouse.is_motion() && !mouse.is_wheel() {
                    *self = Help::Closing;
                }
                true
            }
            (Help::Shown, Action::Bytes(_) | Action::Help) => {
                *self = Help::Hidden;
                true
            }
            (Help::Shown | Help::Closing, _) => {
                *self = Help::Hidden;
                false
            }
        }
    }
}

/// Every command after the prefix. Keys split on " / " are each a command of their own.
const PREFIX_HELP: &[(&str, &str)] = &[
    ("n", "new workspace"),
    ("s / S", "split side by side / stacked"),
    ("x", "close the focused pane"),
    ("o", "focus the next pane"),
    ("arrows", "focus the pane that way"),
    ("1-9", "switch to that workspace"),
    ("g", "open or close the review"),
    ("h", "show this help"),
    ("q", "quit spindle"),
];

const REVIEW_HELP: &[(&str, &str)] = &[
    ("j/k  arrows", "move through the tree"),
    ("h/l", "collapse / expand a folder"),
    ("Enter  o", "toggle a folder"),
    ("g/G", "first / last entry"),
    ("J/K  Space", "scroll the diff"),
    ("r", "reload from git"),
    ("q  Esc", "back to the shell"),
];

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
        pane_inner: pane_inner(pane),
    }
}

/// Where a pane's terminal is drawn inside its border, and so the size its PTY must be.
pub fn pane_inner(pane: Rect) -> Rect {
    Block::bordered().inner(pane)
}

pub struct PaneView<'a> {
    pub area: Rect,
    pub screen: &'a vt100::Screen,
    pub focused: bool,
}

pub struct Sidebar {
    pub workspaces: usize,
    pub active: usize,
    /// Every workspace starts in the launch directory, so they share its name.
    pub dir: String,
    pub prefix: Prefix,
}

struct SidebarAreas {
    menu: Rect,
    rule: Rect,
    lists: Rect,
}

fn sidebar_areas(sidebar: Rect) -> SidebarAreas {
    let [menu, rule, lists] = Layout::vertical([
        Constraint::Length(MENU_HEIGHT),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(Block::bordered().inner(sidebar));
    SidebarAreas { menu, rule, lists }
}

/// How far one scroll key moves the review content: half its visible height.
pub fn review_page(pane_inner: Rect) -> usize {
    (pane_inner.height.saturating_sub(1) / 2).max(1) as usize
}

struct ReviewAreas {
    tree: Rect,
    divider: Rect,
    header: Rect,
    body: Rect,
}

/// Review geometry, shared by drawing and click handling so a click lands on the row drawn there.
fn review_areas(inner: Rect) -> ReviewAreas {
    let tree_width = (inner.width * 35 / 100).clamp(20, 40).min(inner.width);
    let [tree, divider] =
        Layout::horizontal([Constraint::Length(tree_width), Constraint::Min(0)]).areas(inner);
    let content = review_divider().inner(divider);
    let [header, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(content);
    ReviewAreas {
        tree,
        divider,
        header,
        body,
    }
}

fn review_divider() -> Block<'static> {
    Block::new()
        .borders(Borders::LEFT)
        .border_style(Style::new().dark_gray())
}

/// The first tree row on screen: the tree scrolls only as far as needed to keep the cursor visible.
fn tree_offset(review: &Review, height: u16) -> usize {
    review
        .cursor()
        .saturating_sub((height as usize).saturating_sub(1))
}

pub enum Hit {
    MenuKey(u8),
    Workspace(usize),
    TreeRow(usize),
    ReviewPane,
    Nothing,
}

/// What lies under screen cell (`col`, `row`).
pub fn hit(screen: Rect, view: &View, sidebar: &Sidebar, col: u16, row: u16) -> Hit {
    let areas = layout(screen);
    let pos = ratatui::layout::Position::new(col, row);
    let SidebarAreas { menu, lists, .. } = sidebar_areas(areas.sidebar);
    // The workspace rows sit under the WORKSPACES header.
    if lists.contains(pos) && row > lists.y && ((row - lists.y - 1) as usize) < sidebar.workspaces {
        return Hit::Workspace((row - lists.y - 1) as usize);
    }
    if menu.contains(pos) {
        let line = &menu_lines(sidebar.prefix)[(row - menu.y) as usize];
        let mut x = menu.x;
        // A key and the label after it both count as the button.
        let mut key = None;
        for span in &line.spans {
            let width = span.width() as u16;
            if span.style.fg == Some(Color::Cyan) {
                key = span.content.bytes().next();
            }
            if col < x + width {
                return key.map_or(Hit::Nothing, Hit::MenuKey);
            }
            x += width;
        }
        return Hit::Nothing;
    }
    let View::Review(review) = view else {
        return Hit::Nothing;
    };
    if !areas.pane_inner.contains(pos) {
        return Hit::Nothing;
    }
    let tree = review_areas(areas.pane_inner).tree;
    if tree.contains(pos) {
        return Hit::TreeRow(tree_offset(review, tree.height) + (row - tree.y) as usize);
    }
    Hit::ReviewPane
}

pub fn draw(frame: &mut Frame, sidebar: &Sidebar, panes: &[PaneView], view: &View) {
    let areas = layout(frame.area());

    frame.render_widget(Block::bordered().title(" spindle "), areas.sidebar);
    let SidebarAreas { menu, rule, lists } = sidebar_areas(areas.sidebar);
    frame.render_widget(Paragraph::new(menu_lines(sidebar.prefix)), menu);
    frame.render_widget(Block::new().borders(Borders::TOP).dark_gray(), rule);
    frame.render_widget(Paragraph::new(sidebar_lists(sidebar)), lists);

    match view {
        View::Shell => {
            for pane in panes {
                let border = if pane.focused {
                    Style::new().fg(Color::Cyan)
                } else {
                    Style::new()
                };
                frame.render_widget(
                    PseudoTerminal::new(pane.screen).block(Block::bordered().border_style(border)),
                    pane.area,
                );
            }
        }
        View::Review(review) => draw_review(frame, areas.pane, review),
        View::Unavailable(msg) => frame.render_widget(
            Paragraph::new(vec![
                Line::from(msg.as_str()),
                Line::from(""),
                Line::from("Press q or Esc to go back.").dark_gray(),
            ])
            .wrap(Wrap { trim: false })
            .block(Block::bordered()),
            areas.pane,
        ),
    }
}

fn menu_lines(prefix: Prefix) -> Vec<Line<'static>> {
    let key = |k: &'static str| {
        Span::styled(k, Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
    };
    vec![
        Line::from(vec![
            Span::raw(format!("{} ", prefix.caret())).dark_gray(),
            key("n"),
            Span::raw(" new  "),
            key("x"),
            Span::raw(" close"),
        ]),
        Line::from(vec![
            Span::raw("   "),
            key("s"),
            Span::raw(" split  "),
            key("S"),
            Span::raw(" stack"),
        ]),
        Line::from(vec![
            Span::raw("   "),
            key("g"),
            Span::raw(" review "),
            key("q"),
            Span::raw(" quit"),
        ]),
        Line::from(vec![Span::raw("   "), key("h"), Span::raw(" help")]),
    ]
}

fn help_lines(prefix: Prefix) -> Vec<Line<'static>> {
    let section = |title: String| Line::from(title).bold();
    let entry = |keys: String, what: &str| {
        Line::from(vec![
            Span::styled(
                format!("  {keys:<13}"),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw(what.to_owned()),
        ])
    };
    let mut lines = vec![section(format!("{}, then:", prefix.name()))];
    lines.extend(
        PREFIX_HELP
            .iter()
            .map(|(k, what)| entry(k.to_string(), what)),
    );
    lines.push(entry(prefix.name(), "send it to the shell"));
    lines.push(Line::from(""));
    lines.push(section("In the review:".into()));
    lines.extend(
        REVIEW_HELP
            .iter()
            .map(|(k, what)| entry(k.to_string(), what)),
    );
    lines
}

/// Drawn over everything else, after [`draw`], so it covers whichever view is showing.
pub fn draw_help(frame: &mut Frame, prefix: Prefix) {
    let lines = help_lines(prefix);
    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 4;
    let height = lines.len() as u16 + 2;
    let area = frame
        .area()
        .centered(Constraint::Max(width), Constraint::Max(height));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" help ")
                .title_bottom(Line::from(" any key closes ").dark_gray())
                .padding(ratatui::widgets::Padding::horizontal(1)),
        ),
        area,
    );
}

fn draw_review(frame: &mut Frame, area: Rect, review: &Review) {
    let block = Block::bordered()
        .title(format!(" review: {} vs HEAD ", review.root_name()))
        .title_bottom(
            Line::from(" j/k move  h/l fold  J/K scroll  r refresh  q close ").dark_gray(),
        );
    let ReviewAreas {
        tree,
        divider,
        header,
        body,
    } = review_areas(block.inner(area));
    frame.render_widget(block, area);

    // Draws only the visible window so large repos don't build every row each frame.
    let rows: Vec<Line> = review
        .rows()
        .iter()
        .enumerate()
        .skip(tree_offset(review, tree.height))
        .take(tree.height as usize)
        .map(|(i, row)| tree_line(row, i == review.cursor()))
        .collect();
    frame.render_widget(Paragraph::new(rows), tree);

    frame.render_widget(review_divider(), divider);
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

fn sidebar_lists(sidebar: &Sidebar) -> Vec<Line<'static>> {
    let header = |s: &'static str| Line::from(s).bold().dark_gray();
    let agent = |dot: &'static str, name: &'static str, state: &'static str, color: Color| {
        Line::from(vec![
            Span::styled(dot, Style::new().fg(color)),
            Span::raw(format!(" {name:<9}")),
            Span::styled(state, Style::new().fg(color)),
        ])
    };
    let mut lines = vec![header("WORKSPACES")];
    lines.extend((0..sidebar.workspaces).map(|i| {
        let label = format!("{} {}", i + 1, sidebar.dir);
        if i == sidebar.active {
            Line::from(format!("▸ {label}")).bold()
        } else {
            Line::from(format!("  {label}"))
        }
    }));
    // Static stand-ins until agent detection exists.
    lines.extend([
        Line::from(""),
        header("AGENTS"),
        agent("●", "claude", "working", Color::Green),
        agent("◐", "codex", "blocked", Color::Yellow),
        agent("○", "shell", "idle", Color::DarkGray),
    ]);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty;
    use portable_pty::{CommandBuilder, PtySize};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::io::Read;

    fn sidebar(workspaces: usize) -> Sidebar {
        Sidebar {
            workspaces,
            active: 0,
            dir: "spindle".into(),
            prefix: Prefix::default(),
        }
    }

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
            .draw(|f| {
                draw(
                    f,
                    &sidebar(1),
                    &[PaneView {
                        area: layout(area).pane,
                        screen: parser.screen(),
                        focused: false,
                    }],
                    &View::Shell,
                )
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..area.height).map(|y| row(buf, y)).collect();
        println!("{}", rows.join("\n"));

        let sidebar_row = |y: usize| {
            rows[y][..]
                .chars()
                .take(SIDEBAR_WIDTH as usize)
                .collect::<String>()
        };
        assert!(sidebar_row(0).contains("spindle"));
        assert!(
            sidebar_row(1).contains("^b n new  x close"),
            "menu must be the first sidebar row"
        );
        assert!(sidebar_row(2).contains("s split  S stack"));
        assert!(sidebar_row(3).contains("g review q quit"));
        assert!(sidebar_row(4).contains("h help"));
        assert!(sidebar_row(6).contains("WORKSPACES"));
        assert!(sidebar_row(7).contains("▸ 1 spindle"));
        assert!(rows.iter().position(|r| r.contains("AGENTS")).unwrap() > 7);

        let pane_row: String = rows[inner.y as usize]
            .chars()
            .skip(inner.x as usize)
            .collect();
        assert!(pane_row.starts_with("hello from the pane"));
    }

    #[test]
    fn renders_review_view_over_the_pane() {
        let tmp = crate::git::tests::fixture();
        let (review, _rx) = crate::review::tests::open(tmp.path());
        let area = Rect::new(0, 0, 100, 20);

        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|f| draw(f, &sidebar(1), &[], &View::Review(Box::new(review))))
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

    /// The screen cell where `needle` is drawn.
    fn find(rows: &[String], needle: &str) -> (u16, u16) {
        rows.iter()
            .enumerate()
            .find_map(|(y, r)| {
                let byte = r.find(needle)?;
                Some((r[..byte].chars().count() as u16, y as u16))
            })
            .unwrap_or_else(|| panic!("{needle:?} not drawn"))
    }

    #[test]
    fn clicks_on_menu_keys_and_labels_hit_that_key() {
        let area = Rect::new(0, 0, 80, 24);
        let parser = vt100::Parser::new(1, 1, 0);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|f| {
                draw(
                    f,
                    &sidebar(1),
                    &[PaneView {
                        area: layout(area).pane,
                        screen: parser.screen(),
                        focused: false,
                    }],
                    &View::Shell,
                )
            })
            .unwrap();
        let rows: Vec<String> = (0..area.height)
            .map(|y| row(terminal.backend().buffer(), y))
            .collect();

        let key_at = |(x, y): (u16, u16)| match hit(area, &View::Shell, &sidebar(1), x, y) {
            Hit::MenuKey(k) => Some(k as char),
            _ => None,
        };
        let (x, y) = find(&rows, "g review");
        assert_eq!(key_at((x, y)), Some('g'));
        assert_eq!(key_at((x + 4, y)), Some('g'), "label");
        let (x, y) = find(&rows, "q quit");
        assert_eq!(key_at((x, y)), Some('q'));
        assert_eq!(key_at((x + 5, y)), Some('q'), "label");
        let (x, y) = find(&rows, "h help");
        assert_eq!(key_at((x, y)), Some('h'));
        assert_eq!(key_at(find(&rows, "^b")), None);
        assert_eq!(key_at(find(&rows, "WORKSPACES")), None);
    }

    #[test]
    fn menu_shows_the_configured_prefix() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let sidebar = Sidebar {
            prefix: crate::config::tests::prefix("C-a"),
            ..sidebar(1)
        };
        terminal
            .draw(|f| draw(f, &sidebar, &[], &View::Shell))
            .unwrap();
        assert!(row(terminal.backend().buffer(), 1).contains("^a n new"));
    }

    #[test]
    fn clicks_on_workspace_rows_hit_that_workspace() {
        let area = Rect::new(0, 0, 80, 24);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|f| draw(f, &sidebar(2), &[], &View::Shell))
            .unwrap();
        let rows: Vec<String> = (0..area.height)
            .map(|y| row(terminal.backend().buffer(), y))
            .collect();
        let at = |(x, y): (u16, u16)| match hit(area, &View::Shell, &sidebar(2), x, y) {
            Hit::Workspace(i) => Some(i),
            _ => None,
        };
        assert_eq!(at(find(&rows, "1 spindle")), Some(0));
        assert_eq!(at(find(&rows, "2 spindle")), Some(1));
        assert_eq!(at(find(&rows, "WORKSPACES")), None);
        let (x, y) = find(&rows, "2 spindle");
        assert_eq!(at((x, y + 1)), None, "blank row after the list");
    }

    #[test]
    fn clicks_on_tree_rows_hit_the_row_drawn_there_even_when_scrolled() {
        let tmp = crate::git::tests::fixture();
        let (mut review, _rx) = crate::review::tests::open(tmp.path());
        // A pane so short the tree has to scroll to keep the cursor in view.
        let area = Rect::new(0, 0, 100, 6);
        review.handle(crate::review::Key::Bottom, 1);
        let view = View::Review(Box::new(review));
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal.draw(|f| draw(f, &sidebar(1), &[], &view)).unwrap();
        let rows: Vec<String> = (0..area.height)
            .map(|y| row(terminal.backend().buffer(), y))
            .collect();
        println!("{}", rows.join("\n"));
        let View::Review(review) = &view else {
            unreachable!()
        };

        let inner = layout(area).pane_inner;
        let tree = review_areas(inner).tree;
        assert!(
            tree_offset(review, tree.height) > 0,
            "tree should be scrolled"
        );
        for y in tree.y..tree.bottom() {
            let Hit::TreeRow(i) = hit(area, &view, &sidebar(1), tree.x + 1, y) else {
                panic!("row {y} isn't a tree row");
            };
            let drawn: String = rows[y as usize]
                .chars()
                .skip(tree.x as usize)
                .take(tree.width as usize)
                .collect();
            assert!(
                drawn.contains(&review.rows()[i].name()),
                "{drawn:?} vs row {i}"
            );
        }
        let body = review_areas(inner).body;
        assert!(matches!(
            hit(area, &view, &sidebar(1), body.x, body.y),
            Hit::ReviewPane
        ));
    }

    #[test]
    fn file_contents_never_reach_the_buffer_as_escapes() {
        let tmp = crate::git::tests::fixture();
        crate::git::tests::write(tmp.path(), "keep.txt", "evil \x1b[31m\tline\n");
        let (mut review, rx) = crate::review::tests::open(tmp.path());
        while review.selected().unwrap().path != std::path::Path::new("keep.txt") {
            review.handle(crate::review::Key::Down, 1);
        }
        crate::review::tests::settle(&mut review, &rx);
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|f| draw(f, &sidebar(1), &[], &View::Review(Box::new(review))))
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
        let (review, _rx) = crate::review::tests::open(tmp.path());
        assert!(review.selected().unwrap().name().contains("pwned"));

        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        terminal
            .draw(|f| draw(f, &sidebar(1), &[], &View::Review(Box::new(review))))
            .unwrap();
        let buf = terminal.backend().buffer();
        let text: String = (0..10).map(|y| row(buf, y)).collect();
        assert!(text.contains("pwned"));
        assert!(!text.contains('\x1b') && !text.contains('\x07'), "{text:?}");
    }

    #[test]
    fn help_lists_every_prefix_command() {
        let listed: Vec<u8> = PREFIX_HELP
            .iter()
            .flat_map(|(keys, _)| keys.split(" / "))
            .filter(|k| k.len() == 1)
            .map(|k| k.as_bytes()[0])
            .collect();
        for key in &listed {
            assert!(
                crate::input::command(*key).is_some(),
                "{} is listed but unbound",
                *key as char
            );
        }
        for key in b'!'..=b'~' {
            if crate::input::command(key).is_some() && !key.is_ascii_digit() {
                assert!(
                    listed.contains(&key),
                    "{} is bound but unlisted",
                    key as char
                );
            }
        }
    }

    #[test]
    fn help_overlays_the_view_with_the_configured_prefix() {
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        let prefix = crate::config::tests::prefix("C-a");
        terminal
            .draw(|f| {
                draw(f, &sidebar(1), &[], &View::Shell);
                draw_help(f, prefix);
            })
            .unwrap();
        let rows: Vec<String> = (0..30)
            .map(|y| row(terminal.backend().buffer(), y))
            .collect();
        println!("{}", rows.join("\n"));
        let text = rows.join("\n");
        assert!(text.contains("Ctrl-a, then:"));
        assert!(text.contains("show this help"));
        assert!(text.contains("In the review:"));
        assert!(text.contains("any key closes"));

        let mut tiny = Terminal::new(TestBackend::new(20, 5)).unwrap();
        tiny.draw(|f| draw_help(f, prefix)).unwrap();
    }

    #[test]
    fn help_swallows_what_closes_it() {
        use crate::input::Mouse;
        let mouse = |code, release| {
            Action::Mouse(Mouse {
                code,
                col: 0,
                row: 0,
                release,
            })
        };
        let (press, release, wheel) = (mouse(0, false), mouse(0, true), mouse(64, false));

        let mut help = Help::Hidden;
        assert!(!help.swallows(&press), "nothing to close");

        // Opened from the menu: the click's release mustn't close it straight away.
        help = Help::Shown;
        assert!(help.swallows(&release));
        assert!(help.swallows(&wheel));
        assert!(help == Help::Shown);

        assert!(help.swallows(&press));
        assert!(help == Help::Closing);
        assert!(help.swallows(&mouse(32, false)), "drag before the release");
        assert!(help.swallows(&release));
        assert!(help == Help::Hidden);
        assert!(!help.swallows(&press));

        help = Help::Shown;
        assert!(help.swallows(&Action::Bytes(b"a".to_vec())));
        assert!(help == Help::Hidden);

        help = Help::Shown;
        assert!(help.swallows(&Action::Help), "prefix-h again closes it");
        assert!(help == Help::Hidden);

        help = Help::Shown;
        assert!(!help.swallows(&Action::NewWorkspace), "commands still run");
        assert!(help == Help::Hidden);

        help = Help::Closing;
        assert!(
            !help.swallows(&Action::Bytes(b"a".to_vec())),
            "typing after a click"
        );
        assert!(help == Help::Hidden);
    }
}
