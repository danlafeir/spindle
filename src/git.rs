use std::collections::{BTreeMap, BTreeSet};
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
/// The same cap the review puts on reading a whole file.
const MAX_DIFF_BYTES: u64 = 1024 * 1024;

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

pub struct Status {
    pub changes: BTreeMap<PathBuf, Change>,
    /// Staged as new and then deleted: in neither HEAD nor the working tree, so not a change,
    /// but still in the index, where `ls-files` keeps listing them.
    pub phantoms: BTreeSet<PathBuf>,
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

/// Reads up to `limit` bytes, plus one so the caller can tell it was cut short. The rest is
/// read and thrown away: git must be able to finish writing, or it would block until killed.
fn drain(mut pipe: impl Read + Send + 'static, limit: u64) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe
            .by_ref()
            .take(limit.saturating_add(1))
            .read_to_end(&mut buf);
        let _ = std::io::copy(&mut pipe, &mut std::io::sink());
        buf
    })
}

fn run(dir: &Path, args: &[&str]) -> Result<Output> {
    run_capped(dir, args, &[], u64::MAX)
}

/// `paths` go after `args` as they are, since a file name needn't be valid UTF-8.
fn run_capped(dir: &Path, args: &[&str], paths: &[&Path], stdout_limit: u64) -> Result<Output> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        // An fsmonitor hook in a repo's own .git/config would otherwise run on every `git status`.
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .args(paths)
        // Read-only views must not take index.lock and collide with git running in the shell.
        .env("GIT_OPTIONAL_LOCKS", "0")
        // Paths come from git's own listings, so `[`, `*` or `?` in a name is literal, never a glob.
        .env("GIT_LITERAL_PATHSPECS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run git")?;
    // Drained concurrently: a diff larger than the pipe buffer would otherwise block git forever.
    let stdout = drain(child.stdout.take().expect("stdout is piped"), stdout_limit);
    let stderr = drain(child.stderr.take().expect("stderr is piped"), u64::MAX);

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

fn parse_status(bytes: &[u8]) -> Status {
    let mut changes = BTreeMap::new();
    let mut phantoms = BTreeSet::new();
    let mut entries = bytes.split(|&b| b == 0);
    while let Some(entry) = entries.next() {
        // Skips the empty field after the final NUL. Only entries are checked: a rename's
        // source path is read below and can be as short as one byte.
        if entry.len() < 4 {
            continue;
        }
        let (x, y) = (entry[0], entry[1]);
        let path = PathBuf::from(OsStr::from_bytes(&entry[3..]));
        let change = match (x, y) {
            (b'A', b'D') => {
                phantoms.insert(path);
                continue;
            }
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
    Status { changes, phantoms }
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

    pub fn status(&self) -> Result<Status> {
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
        let base = [
            "--no-pager",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
        ];
        let (args, paths) = match change {
            Change::Untracked => (
                [&base[..], &["--no-index", "--"]].concat(),
                vec![Path::new("/dev/null"), path],
            ),
            Change::Renamed { from } => (
                [&base[..], &["-M", "HEAD", "--"]].concat(),
                vec![from.as_path(), path],
            ),
            _ => ([&base[..], &["HEAD", "--"]].concat(), vec![path]),
        };
        let out = run_capped(&self.root, &args, &paths, MAX_DIFF_BYTES)?;
        // --no-index exits 1 to mean "files differ", which is the expected outcome for untracked files.
        if !matches!(out.code, Some(0 | 1)) {
            return Err(out.error("git diff"));
        }
        let mut stdout = out.stdout;
        let truncated = stdout.len() as u64 > MAX_DIFF_BYTES;
        stdout.truncate(MAX_DIFF_BYTES as usize);
        let mut diff = String::from_utf8_lossy(&stdout).into_owned();
        if truncated {
            diff.push_str("\n… truncated at 1 MB");
        }
        Ok(diff)
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
        let changes = repo.status().unwrap().changes;
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
    fn short_rename_source_does_not_swallow_the_next_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q"]);
        write(dir, "a.c", "same contents for rename detection\n");
        write(dir, "mod.txt", "x\n");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "init"]);
        git(dir, &["mv", "a.c", "b.c"]);
        write(dir, "mod.txt", "changed\n");

        let changes = Repo::discover(dir).unwrap().status().unwrap().changes;
        assert_eq!(
            changes.get(Path::new("b.c")),
            Some(&Change::Renamed { from: "a.c".into() })
        );
        assert_eq!(changes.get(Path::new("mod.txt")), Some(&Change::Modified));
    }

    #[test]
    fn added_then_deleted_is_not_a_change() {
        let tmp = fixture();
        write(tmp.path(), "brief.txt", "x\n");
        git(tmp.path(), &["add", "brief.txt"]);
        fs::remove_file(tmp.path().join("brief.txt")).unwrap();

        let status = Repo::discover(tmp.path()).unwrap().status().unwrap();
        assert_eq!(status.changes.get(Path::new("brief.txt")), None);
        assert!(status.phantoms.contains(Path::new("brief.txt")));
    }

    #[test]
    fn huge_diffs_are_cut_at_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q"]);
        let line = "x".repeat(99) + "\n";
        write(dir, "big.txt", &line.repeat(30_000));

        let diff = Repo::discover(dir)
            .unwrap()
            .diff(Path::new("big.txt"), &Change::Untracked)
            .unwrap();
        assert!(diff.ends_with("… truncated at 1 MB"));
        assert!(diff.len() < MAX_DIFF_BYTES as usize + 100);
    }

    #[test]
    fn glob_characters_in_a_name_match_only_that_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q"]);
        write(dir, "a[b].txt", "one\n");
        write(dir, "ab.txt", "two\n");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "init"]);
        write(dir, "a[b].txt", "one changed\n");
        write(dir, "ab.txt", "two changed\n");

        let diff = Repo::discover(dir)
            .unwrap()
            .diff(Path::new("a[b].txt"), &Change::Modified)
            .unwrap();
        assert!(diff.contains("+one changed"));
        assert!(!diff.contains("ab.txt"), "{diff}");
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
        let changes = repo.status().unwrap().changes;
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

    #[test]
    fn a_repos_own_config_cannot_run_commands() {
        let tmp = fixture();
        let dir = tmp.path();
        let marker = |name: &str| dir.join(".git").join(name);
        let touch = |name: &str| format!("touch '{}'", marker(name).display());
        git(dir, &["config", "core.fsmonitor", &touch("fsmonitor")]);
        git(
            dir,
            &[
                "config",
                "diff.evil.textconv",
                &format!("{}; cat", touch("textconv")),
            ],
        );
        write(dir, ".gitattributes", "*.txt diff=evil\n");

        let repo = Repo::discover(dir).unwrap();
        let changes = repo.status().unwrap().changes;
        repo.diff(Path::new("mod.txt"), &changes[Path::new("mod.txt")])
            .unwrap();
        assert!(!marker("fsmonitor").exists(), "fsmonitor hook ran");
        assert!(!marker("textconv").exists(), "textconv filter ran");
    }

    #[test]
    fn diffs_files_whose_names_are_not_utf8() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q"]);
        write(dir, "blob", "x\n");
        let sha = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["hash-object", "-w", "blob"])
            .output()
            .unwrap()
            .stdout;
        let sha = String::from_utf8(sha).unwrap();
        // macOS refuses such names on disk, so the file exists only in the commit: a deletion.
        let name = OsStr::from_bytes(b"caf\xe9.txt");
        let mut cacheinfo = format!("100644,{},", sha.trim()).into_bytes();
        cacheinfo.extend(name.as_bytes());
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["update-index", "--add", "--cacheinfo"])
            .arg(OsStr::from_bytes(&cacheinfo))
            .status()
            .unwrap();
        assert!(status.success());
        git(dir, &["commit", "-q", "-m", "init"]);

        let repo = Repo::discover(dir).unwrap();
        let changes = repo.status().unwrap().changes;
        assert_eq!(changes.get(Path::new(name)), Some(&Change::Deleted));
        let diff = repo.diff(Path::new(name), &Change::Deleted).unwrap();
        assert!(diff.contains("-x"), "{diff:?}");
    }
}
