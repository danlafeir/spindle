use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use wait_timeout::ChildExt;

const TIMEOUT: Duration = Duration::from_secs(5);

/// How a path differs between HEAD and the working tree, staged and unstaged combined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Modified,
    Added,
    Deleted,
    Renamed { from: PathBuf },
    Untracked,
}

impl Change {
    pub fn marker(&self) -> char {
        match self {
            Change::Modified => 'M',
            Change::Added => 'A',
            Change::Deleted => 'D',
            Change::Renamed { .. } => 'R',
            Change::Untracked => '?',
        }
    }
}

pub struct Repo {
    root: PathBuf,
}

struct Output {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Output {
    fn error(&self, what: &str) -> anyhow::Error {
        let msg = String::from_utf8_lossy(&self.stderr);
        anyhow::anyhow!("{what} failed: {}", msg.trim())
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

fn run(dir: &Path, args: &[&str]) -> Result<Output> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        // Read-only views must not take index.lock and collide with git running in the shell.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run git")?;
    // Drained concurrently: a diff larger than the pipe buffer would otherwise block git forever.
    let stdout = drain(child.stdout.take().expect("stdout is piped"));
    let stderr = drain(child.stderr.take().expect("stderr is piped"));

    let Some(status) = child.wait_timeout(TIMEOUT)? else {
        let _ = child.kill();
        let _ = child.wait();
        bail!(
            "git {} timed out after {}s",
            args.join(" "),
            TIMEOUT.as_secs()
        );
    };
    Ok(Output {
        code: status.code(),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn nul_paths(bytes: &[u8]) -> impl Iterator<Item = PathBuf> + '_ {
    bytes
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(OsStr::from_bytes(p)))
}

fn parse_status(bytes: &[u8]) -> BTreeMap<PathBuf, Change> {
    let mut changes = BTreeMap::new();
    let mut entries = bytes.split(|&b| b == 0).filter(|e| e.len() > 3);
    while let Some(entry) = entries.next() {
        let (x, y) = (entry[0], entry[1]);
        let path = PathBuf::from(OsStr::from_bytes(&entry[3..]));
        let change = match (x, y) {
            (b'?', b'?') => Change::Untracked,
            // -z puts a rename's or copy's source path in the next NUL-separated field.
            (b'R', _) | (b'C', _) => {
                let from = entries.next().map(|p| PathBuf::from(OsStr::from_bytes(p)));
                match (x, from) {
                    (b'R', Some(from)) => Change::Renamed { from },
                    _ => Change::Added,
                }
            }
            (b'D', _) | (_, b'D') => Change::Deleted,
            (b'A', _) => Change::Added,
            _ => Change::Modified,
        };
        changes.insert(path, change);
    }
    changes
}

impl Repo {
    pub fn discover(dir: &Path) -> Result<Repo> {
        let out = run(dir, &["rev-parse", "--show-toplevel"])?;
        if out.code != Some(0) {
            bail!("{} is not inside a git repository", dir.display());
        }
        let root = String::from_utf8_lossy(&out.stdout).trim_end().to_owned();
        Ok(Repo { root: root.into() })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn changes(&self) -> Result<BTreeMap<PathBuf, Change>> {
        let out = run(
            &self.root,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )?;
        if out.code != Some(0) {
            return Err(out.error("git status"));
        }
        Ok(parse_status(&out.stdout))
    }

    /// Tracked and untracked files, minus anything gitignored.
    pub fn files(&self) -> Result<Vec<PathBuf>> {
        let out = run(
            &self.root,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        )?;
        if out.code != Some(0) {
            return Err(out.error("git ls-files"));
        }
        Ok(nul_paths(&out.stdout).collect())
    }

    pub fn diff(&self, path: &Path, change: &Change) -> Result<String> {
        let path = path.to_string_lossy();
        let base = ["--no-pager", "diff", "--no-color", "--no-ext-diff"];
        let out = match change {
            Change::Untracked => {
                let args = [&base[..], &["--no-index", "--", "/dev/null", &path]].concat();
                run(&self.root, &args)?
            }
            Change::Renamed { from } => {
                let from = from.to_string_lossy();
                let args = [&base[..], &["-M", "HEAD", "--", &from, &path]].concat();
                run(&self.root, &args)?
            }
            _ => run(&self.root, &[&base[..], &["HEAD", "--", &path]].concat())?,
        };
        // --no-index exits 1 to mean "files differ", which is the expected outcome for untracked files.
        if !matches!(out.code, Some(0 | 1)) {
            return Err(out.error("git diff"));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::fs;

    /// Runs git isolated from the user's config so their signing or hooks can't break setup.
    pub fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=spindle", "-c", "user.email=spindle@test"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    pub fn write(dir: &Path, path: &str, contents: &str) {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// A repo with one of every kind of change spindle needs to show.
    pub fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q"]);
        for f in ["keep.txt", "mod.txt", "gone.txt", "staged_gone.txt"] {
            write(dir, f, &format!("{f}\n"));
        }
        write(dir, "old_name.txt", "same contents for rename detection\n");
        write(dir, "src/dir file.txt", "spaced\n");
        write(dir, ".gitignore", "target/\n");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "init"]);

        write(dir, "mod.txt", "changed\n");
        write(dir, "src/dir file.txt", "spaced and changed\n");
        fs::remove_file(dir.join("gone.txt")).unwrap();
        git(dir, &["rm", "-q", "staged_gone.txt"]);
        git(dir, &["mv", "old_name.txt", "new_name.txt"]);
        write(dir, "src/new.rs", "fn main() {}\n");
        write(dir, "target/junk", "ignored\n");
        tmp
    }

    #[test]
    fn changes_cover_every_kind_vs_head() {
        let tmp = fixture();
        let repo = Repo::discover(&tmp.path().join("src")).unwrap();
        let changes = repo.changes().unwrap();
        let get = |p: &str| changes.get(Path::new(p)).cloned();

        assert_eq!(get("mod.txt"), Some(Change::Modified));
        assert_eq!(get("src/dir file.txt"), Some(Change::Modified));
        assert_eq!(get("gone.txt"), Some(Change::Deleted));
        assert_eq!(get("staged_gone.txt"), Some(Change::Deleted));
        assert_eq!(
            get("new_name.txt"),
            Some(Change::Renamed {
                from: "old_name.txt".into()
            })
        );
        assert_eq!(get("src/new.rs"), Some(Change::Untracked));
        assert_eq!(get("keep.txt"), None);
        assert_eq!(changes.len(), 6, "{changes:?}");
    }

    #[test]
    fn files_hide_gitignored_paths() {
        let tmp = fixture();
        let files = Repo::discover(tmp.path()).unwrap().files().unwrap();
        assert!(files.contains(&"keep.txt".into()));
        assert!(files.contains(&"src/new.rs".into()));
        assert!(!files.iter().any(|f| f.starts_with("target")));
    }

    #[test]
    fn diffs_each_kind_of_change() {
        let tmp = fixture();
        let repo = Repo::discover(tmp.path()).unwrap();
        let changes = repo.changes().unwrap();
        let diff = |p: &str| repo.diff(Path::new(p), &changes[Path::new(p)]).unwrap();

        assert!(diff("mod.txt").contains("+changed"));
        assert!(diff("src/dir file.txt").contains("+spaced and changed"));
        assert!(diff("gone.txt").contains("-gone.txt"));
        assert!(diff("src/new.rs").contains("+fn main() {}"));
        assert!(diff("new_name.txt").contains("rename from old_name.txt"));
    }

    #[test]
    fn diff_reports_missing_head_as_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]);
        write(tmp.path(), "a.txt", "a\n");
        git(tmp.path(), &["add", "a.txt"]);
        let repo = Repo::discover(tmp.path()).unwrap();
        assert!(repo.diff(Path::new("a.txt"), &Change::Added).is_err());
    }

    #[test]
    fn discover_outside_a_repo_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(Repo::discover(tmp.path()).is_err());
    }
}
