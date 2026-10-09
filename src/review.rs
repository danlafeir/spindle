use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::git::{Change, Repo};

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const TAB_WIDTH: usize = 4;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Top,
    Bottom,
    ScrollDown,
    ScrollUp,
    Refresh,
    Close,
}

/// Parses raw terminal bytes into review keys, accepting both CSI (`ESC [ A`) and SS3
/// (`ESC O A`) arrows. A read that is exactly one ESC is the Esc key; anywhere else ESC starts
/// a sequence, so unknown sequences are skipped whole rather than misread as letters.
pub fn parse_keys(bytes: &[u8]) -> Vec<Key> {
    if bytes == [0x1b] {
        return vec![Key::Close];
    }
    let mut keys = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            let seq_end = bytes[i + 1..]
                .iter()
                .skip(1)
                .position(|b| (0x40..=0x7e).contains(b))
                .map_or(bytes.len(), |p| i + 2 + p + 1);
            let key = match &bytes[i + 1..seq_end.min(bytes.len())] {
                [b'[' | b'O', b'A'] => Some(Key::Up),
                [b'[' | b'O', b'B'] => Some(Key::Down),
                [b'[' | b'O', b'C'] => Some(Key::Right),
                [b'[' | b'O', b'D'] => Some(Key::Left),
                [b'[', b'5', b'~'] => Some(Key::ScrollUp),
                [b'[', b'6', b'~'] => Some(Key::ScrollDown),
                _ => None,
            };
            keys.extend(key);
            i = seq_end;
            continue;
        }
        let key = match bytes[i] {
            b'j' => Some(Key::Down),
            b'k' => Some(Key::Up),
            b'h' => Some(Key::Left),
            b'l' => Some(Key::Right),
            b'\r' | b'\n' | b'o' => Some(Key::Enter),
            b'g' => Some(Key::Top),
            b'G' => Some(Key::Bottom),
            b'J' | b' ' | 0x04 => Some(Key::ScrollDown),
            b'K' | 0x15 => Some(Key::ScrollUp),
            b'r' => Some(Key::Refresh),
            b'q' => Some(Key::Close),
            _ => None,
        };
        keys.extend(key);
        i += 1;
    }
    keys
}

#[derive(Default)]
struct Node {
    dirs: BTreeMap<OsString, Node>,
    files: BTreeSet<OsString>,
}

impl Node {
    fn insert(&mut self, path: &Path) {
        let mut node = self;
        let mut parts = path.iter().peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                node.files.insert(part.to_owned());
            } else {
                node = node.dirs.entry(part.to_owned()).or_default();
            }
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum RowKind {
    Dir { open: bool, changed: bool },
    File { change: Option<Change> },
}

#[derive(Debug)]
pub struct Row {
    pub path: PathBuf,
    pub depth: usize,
    pub kind: RowKind,
}

impl Row {
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    }
}

pub enum Content {
    Diff(Vec<String>),
    File(Vec<String>),
    Message(String),
}

pub enum Outcome {
    Stay,
    Close,
}

pub struct Review {
    repo: Repo,
    tree: Node,
    changes: BTreeMap<PathBuf, Change>,
    changed_dirs: HashSet<PathBuf>,
    open_dirs: HashSet<PathBuf>,
    rows: Vec<Row>,
    cursor: usize,
    content: Content,
    scroll: usize,
}

impl Review {
    /// Opens the repo containing `dir` with every folder that holds a change expanded and the
    /// cursor on the first changed file.
    pub fn open(dir: &Path) -> Result<Review> {
        let repo = Repo::discover(dir)?;
        let mut review = Review {
            repo,
            tree: Node::default(),
            changes: BTreeMap::new(),
            changed_dirs: HashSet::new(),
            open_dirs: HashSet::new(),
            rows: Vec::new(),
            cursor: 0,
            content: Content::Message(String::new()),
            scroll: 0,
        };
        review.load()?;
        review.open_dirs = review.changed_dirs.clone();
        review.rebuild_rows();
        review.cursor = review
            .rows
            .iter()
            .position(|r| matches!(r.kind, RowKind::File { change: Some(_) }))
            .unwrap_or(0);
        review.select();
        Ok(review)
    }

    pub fn root_name(&self) -> String {
        self.repo
            .root()
            .file_name()
            .map_or_else(|| "/".into(), |n| n.to_string_lossy().into_owned())
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn content(&self) -> &Content {
        &self.content
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn selected(&self) -> Option<&Row> {
        self.rows.get(self.cursor)
    }

    fn load(&mut self) -> Result<()> {
        let status = self.repo.status()?;
        let changes = status.changes;
        // Staged deletions are gone from ls-files but still need a row.
        let mut files: BTreeSet<PathBuf> = self.repo.files()?.into_iter().collect();
        files.extend(changes.keys().cloned());
        files.retain(|f| !status.phantoms.contains(f));

        let mut tree = Node::default();
        for f in &files {
            tree.insert(f);
        }
        self.changed_dirs = changes
            .keys()
            .flat_map(|p| p.ancestors().skip(1))
            .filter(|a| !a.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .collect();
        self.tree = tree;
        self.changes = changes;
        Ok(())
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        self.walk(&self.tree, Path::new(""), 0, &mut rows);
        self.rows = rows;
    }

    fn walk(&self, node: &Node, prefix: &Path, depth: usize, rows: &mut Vec<Row>) {
        for (name, child) in &node.dirs {
            let path = prefix.join(name);
            let open = self.open_dirs.contains(&path);
            rows.push(Row {
                path: path.clone(),
                depth,
                kind: RowKind::Dir {
                    open,
                    changed: self.changed_dirs.contains(&path),
                },
            });
            if open {
                self.walk(child, &path, depth + 1, rows);
            }
        }
        for name in &node.files {
            let path = prefix.join(name);
            let change = self.changes.get(&path).cloned();
            rows.push(Row {
                path,
                depth,
                kind: RowKind::File { change },
            });
        }
    }

    fn select(&mut self) {
        self.scroll = 0;
        self.content = match self.rows.get(self.cursor) {
            None => Content::Message("No files.".into()),
            Some(Row {
                kind: RowKind::Dir { .. },
                path,
                ..
            }) => Content::Message(format!("{}/", path.display())),
            Some(Row {
                kind: RowKind::File {
                    change: Some(change),
                },
                path,
                ..
            }) => match self.repo.diff(path, change) {
                Ok(diff) => Content::Diff(diff.lines().map(clean).collect()),
                Err(e) => Content::Message(format!("{e:#}")),
            },
            Some(Row {
                kind: RowKind::File { change: None },
                path,
                ..
            }) => read_file(&self.repo.root().join(path)),
        };
    }

    fn move_to(&mut self, cursor: usize) {
        let cursor = cursor.min(self.rows.len().saturating_sub(1));
        if cursor != self.cursor {
            self.cursor = cursor;
            self.select();
        }
    }

    fn set_dir_open(&mut self, open: bool) {
        let Some(Row {
            path,
            kind: RowKind::Dir { open: was_open, .. },
            ..
        }) = self.rows.get(self.cursor)
        else {
            return;
        };
        if *was_open == open {
            return;
        }
        let path = path.clone();
        if open {
            self.open_dirs.insert(path);
        } else {
            self.open_dirs.remove(&path);
        }
        self.rebuild_rows();
    }

    fn refresh(&mut self) {
        let selected = self.selected().map(|r| r.path.clone());
        if let Err(e) = self.load() {
            self.content = Content::Message(format!("{e:#}"));
            return;
        }
        self.rebuild_rows();
        self.cursor = selected
            .and_then(|p| self.rows.iter().position(|r| r.path == p))
            .unwrap_or(self.cursor)
            .min(self.rows.len().saturating_sub(1));
        self.select();
    }

    /// Selects `row` the way a mouse click would: a folder also toggles open or closed.
    pub fn click(&mut self, row: usize) {
        if row >= self.rows.len() {
            return;
        }
        self.move_to(row);
        self.handle(Key::Enter, 0);
    }

    /// `page` is how many lines a scroll key moves the content.
    pub fn handle(&mut self, key: Key, page: usize) -> Outcome {
        match key {
            Key::Close => return Outcome::Close,
            Key::Down => self.move_to(self.cursor + 1),
            Key::Up => self.move_to(self.cursor.saturating_sub(1)),
            Key::Top => self.move_to(0),
            Key::Bottom => self.move_to(usize::MAX),
            Key::Right => self.set_dir_open(true),
            Key::Enter => {
                if let Some(Row {
                    kind: RowKind::Dir { open, .. },
                    ..
                }) = self.selected()
                {
                    let open = !*open;
                    self.set_dir_open(open);
                }
            }
            Key::Left => {
                if let Some(Row {
                    kind: RowKind::Dir { open: true, .. },
                    ..
                }) = self.selected()
                {
                    self.set_dir_open(false);
                } else if let Some(parent) = self.selected().and_then(|r| r.path.parent()) {
                    let parent = parent.to_path_buf();
                    if let Some(i) = self.rows.iter().position(|r| r.path == parent) {
                        self.move_to(i);
                    }
                }
            }
            Key::ScrollDown => {
                let len = match &self.content {
                    Content::Diff(l) | Content::File(l) => l.len(),
                    Content::Message(_) => 0,
                };
                self.scroll = (self.scroll + page).min(len.saturating_sub(1));
            }
            Key::ScrollUp => self.scroll = self.scroll.saturating_sub(page),
            Key::Refresh => self.refresh(),
        }
        Outcome::Stay
    }
}

fn read_file(path: &Path) -> Content {
    // A repo's files come from whoever wrote it, so a committed symlink is shown, not followed:
    // one to /dev/tty would block the event loop forever, and one to ~/.ssh/config would put a
    // file from outside the repo on screen.
    let kind = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta.file_type(),
        Err(e) => return Content::Message(format!("Can't read {}: {e}", path.display())),
    };
    if kind.is_symlink() {
        let target = std::fs::read_link(path).map_or_else(
            |e| format!("an unreadable target ({e})"),
            |t| clean(&t.to_string_lossy()),
        );
        return Content::Message(format!("Symlink to {target}"));
    }
    if kind.is_dir() {
        return Content::Message("Submodule; nothing to show.".into());
    }
    if !kind.is_file() {
        return Content::Message("Not a regular file; nothing to show.".into());
    }
    let mut bytes = Vec::new();
    let read = File::open(path).and_then(|f| f.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes));
    if let Err(e) = read {
        return Content::Message(format!("Can't read {}: {e}", path.display()));
    }
    if bytes.contains(&0) {
        return Content::Message("Binary file; nothing to show.".into());
    }
    let truncated = bytes.len() as u64 > MAX_FILE_BYTES;
    bytes.truncate(MAX_FILE_BYTES as usize);
    let mut lines: Vec<String> = String::from_utf8_lossy(&bytes).lines().map(clean).collect();
    if truncated {
        lines.push("… truncated at 1 MB".into());
    }
    Content::File(lines)
}

/// Makes a line safe to draw: tabs expand to stops, and control characters (including ESC,
/// which would otherwise let file contents drive the outer terminal) become visible symbols.
pub fn clean(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut col = 0;
    for c in line.chars() {
        match c {
            '\t' => {
                let pad = TAB_WIDTH - col % TAB_WIDTH;
                out.extend(std::iter::repeat_n(' ', pad));
                col += pad;
            }
            '\r' => {}
            c if c.is_control() => {
                out.push(char::REPLACEMENT_CHARACTER);
                col += 1;
            }
            c => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::tests::{fixture, write};

    fn names(review: &Review) -> Vec<String> {
        review
            .rows()
            .iter()
            .map(|r| format!("{}{}", "  ".repeat(r.depth), r.name()))
            .collect()
    }

    #[test]
    fn parses_letters_arrows_and_lone_escape() {
        assert_eq!(parse_keys(b"jk"), vec![Key::Down, Key::Up]);
        assert_eq!(parse_keys(b"\x1b[A\x1bOB"), vec![Key::Up, Key::Down]);
        assert_eq!(parse_keys(b"\x1b[6~"), vec![Key::ScrollDown]);
        assert_eq!(parse_keys(b"\x1b"), vec![Key::Close]);
        // An unknown sequence is skipped whole, not read as the letters inside it.
        assert_eq!(parse_keys(b"\x1b[1;5Dj"), vec![Key::Down]);
    }

    #[test]
    fn opens_with_changes_expanded_and_first_change_selected() {
        let tmp = fixture();
        let review = Review::open(tmp.path()).unwrap();
        assert_eq!(
            names(&review),
            [
                "src",
                "  dir file.txt",
                "  new.rs",
                ".gitignore",
                "gone.txt",
                "keep.txt",
                "mod.txt",
                "new_name.txt",
                "staged_gone.txt",
            ]
        );
        assert_eq!(
            review.selected().unwrap().path,
            Path::new("src/dir file.txt")
        );
        assert!(
            matches!(review.content(), Content::Diff(l) if l.iter().any(|l| l == "+spaced and changed"))
        );
    }

    #[test]
    fn folding_and_navigation() {
        let tmp = fixture();
        let mut review = Review::open(tmp.path()).unwrap();
        review.handle(Key::Left, 10);
        assert_eq!(review.selected().unwrap().path, Path::new("src"));
        review.handle(Key::Left, 10);
        assert_eq!(names(&review)[1], ".gitignore");
        review.handle(Key::Enter, 10);
        assert_eq!(names(&review)[1], "  dir file.txt");

        review.handle(Key::Bottom, 10);
        assert_eq!(
            review.selected().unwrap().path,
            Path::new("staged_gone.txt")
        );
        review.handle(Key::Up, 10);
        review.handle(Key::Up, 10);
        review.handle(Key::Up, 10);
        assert_eq!(review.selected().unwrap().path, Path::new("keep.txt"));
        assert!(matches!(review.content(), Content::File(l) if l == &["keep.txt"]));
    }

    #[test]
    fn clicks_select_files_and_toggle_folders() {
        let tmp = fixture();
        let mut review = Review::open(tmp.path()).unwrap();
        review.click(5);
        assert_eq!(review.selected().unwrap().path, Path::new("keep.txt"));
        review.click(0);
        assert_eq!(names(&review)[1], ".gitignore", "src/ folds");
        review.click(0);
        assert_eq!(names(&review)[1], "  dir file.txt", "and unfolds");
        review.click(99);
        assert_eq!(review.selected().unwrap().path, Path::new("src"));
    }

    #[test]
    fn added_then_deleted_files_leave_the_tree() {
        let tmp = fixture();
        crate::git::tests::write(tmp.path(), "brief.txt", "x\n");
        crate::git::tests::git(tmp.path(), &["add", "brief.txt"]);
        std::fs::remove_file(tmp.path().join("brief.txt")).unwrap();
        let review = Review::open(tmp.path()).unwrap();
        assert!(!names(&review).iter().any(|n| n.contains("brief.txt")));
    }

    #[test]
    fn refresh_picks_up_new_changes_and_keeps_selection() {
        let tmp = fixture();
        let mut review = Review::open(tmp.path()).unwrap();
        write(tmp.path(), "keep.txt", "edited\n");
        review.handle(Key::Refresh, 10);
        assert_eq!(
            review.selected().unwrap().path,
            Path::new("src/dir file.txt")
        );
        let keep = review
            .rows()
            .iter()
            .find(|r| r.path == Path::new("keep.txt"))
            .unwrap();
        assert_eq!(
            keep.kind,
            RowKind::File {
                change: Some(Change::Modified)
            }
        );
    }

    #[test]
    fn symlinks_are_described_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink("/dev/tty", &link).unwrap();
        assert!(
            matches!(read_file(&link), Content::Message(m) if m == "Symlink to /dev/tty"),
            "reading through it would block on the terminal"
        );

        std::fs::write(tmp.path().join("secret"), "outside the repo").unwrap();
        let out = tmp.path().join("out");
        std::os::unix::fs::symlink(tmp.path().join("secret"), &out).unwrap();
        assert!(matches!(read_file(&out), Content::Message(m) if m.starts_with("Symlink to ")));
    }

    #[test]
    fn special_files_are_not_opened() {
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join("fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        // Opening a FIFO with no writer would block forever.
        assert!(matches!(
            read_file(&fifo),
            Content::Message(m) if m == "Not a regular file; nothing to show."
        ));
    }

    #[test]
    fn committed_symlink_selected_on_open_is_described() {
        let tmp = tempfile::tempdir().unwrap();
        crate::git::tests::git(tmp.path(), &["init", "-q"]);
        std::os::unix::fs::symlink("/dev/tty", tmp.path().join("a")).unwrap();
        crate::git::tests::git(tmp.path(), &["add", "."]);
        crate::git::tests::git(tmp.path(), &["commit", "-q", "-m", "init"]);
        let review = Review::open(tmp.path()).unwrap();
        assert_eq!(review.selected().unwrap().path, Path::new("a"));
        assert!(matches!(review.content(), Content::Message(m) if m == "Symlink to /dev/tty"));
    }

    #[test]
    fn binary_files_are_not_shown() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("bin"), b"a\0b").unwrap();
        assert!(matches!(
            read_file(&tmp.path().join("bin")),
            Content::Message(_)
        ));
    }

    #[test]
    fn clean_expands_tabs_and_neutralises_escapes() {
        assert_eq!(clean("a\tb"), "a   b");
        assert_eq!(clean("\x1b[31mred\r"), "\u{fffd}[31mred");
    }
}
