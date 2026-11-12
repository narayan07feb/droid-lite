//! GitLens-style data from the `git` command: line blame (of the editor text, unsaved edits
//! included), file history and one commit's diff.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const UNCOMMITTED: &str = "0000000000000000000000000000000000000000";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub author: String,
    pub email: String,
    /// Unix seconds.
    pub time: i64,
    pub summary: String,
}

impl Commit {
    pub fn is_uncommitted(&self) -> bool {
        self.sha == UNCOMMITTED
    }
    pub fn short(&self) -> &str {
        &self.sha[..self.sha.len().min(8)]
    }
}

#[derive(Clone, Debug, Default)]
pub struct Blame {
    /// For each line (0-based), an index into `commits`.
    pub lines: Vec<usize>,
    pub commits: Vec<Commit>,
}

impl Blame {
    pub fn line(&self, i: usize) -> Option<&Commit> {
        self.lines.get(i).map(|c| &self.commits[*c])
    }
}

fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        // background status/blame must never take index.lock: the user's own git commands
        // ("Another git process seems to be running") would fail while Droid Lite refreshes
        .env("GIT_OPTIONAL_LOCKS", "0");
    c
}

/// Blame of `contents` (the editor text) for `path`. `None` when the file is not tracked by git.
/// One uncommitted change in a file, compared with HEAD: `new_len` lines from `new_start`
/// (0-based) replaced `old` (the lines from HEAD). `new_len == 0` is a deletion, `old` empty an addition.
#[derive(Clone, Debug, PartialEq)]
pub struct Hunk {
    pub new_start: usize,
    pub new_len: usize,
    pub old: Vec<String>,
}

impl Hunk {
    pub fn kind(&self) -> char {
        match (self.old.is_empty(), self.new_len) {
            (true, _) => 'A',
            (false, 0) => 'D',
            _ => 'M',
        }
    }
}

/// The file's text at HEAD; `None` when it is not tracked (then every line is new).
pub fn head_text(path: &Path) -> Option<String> {
    let (dir, name) = (path.parent()?, path.file_name()?);
    let out = git(dir)
        .arg("show")
        .arg(format!("HEAD:./{}", name.to_string_lossy()))
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Line changes from `old` to `new`, through `git diff --no-index -U0`.
pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    if old == new {
        return Vec::new();
    }
    let dir = std::env::temp_dir().join(format!(
        "droid-lite-hunks-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let (a, b) = (dir.join("old"), dir.join("new"));
    if std::fs::write(&a, old).is_err() || std::fs::write(&b, new).is_err() {
        return Vec::new();
    }
    let out = Command::new("git")
        .args(["diff", "--no-index", "--no-color", "-U0", "--"])
        .arg(&a)
        .arg(&b)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
    let _ = std::fs::remove_dir(&dir);
    let mut v: Vec<Hunk> = Vec::new();
    for line in out.lines() {
        if let Some(h) = line.strip_prefix("@@ -") {
            // @@ -old_start[,old_len] +new_start[,new_len] @@
            let mut parts = h.split(' ');
            let range = |p: Option<&str>| -> (usize, usize) {
                let p = p.unwrap_or("0").trim_start_matches(['-', '+']);
                let (s, l) = p.split_once(',').unwrap_or((p, "1"));
                (s.parse().unwrap_or(0), l.parse().unwrap_or(1))
            };
            let _old = range(parts.next());
            let (ns, nl) = range(parts.next());
            // for a deletion git gives the line *before* the gap; 0-based that is the gap's index
            let new_start = if nl == 0 { ns } else { ns.saturating_sub(1) };
            v.push(Hunk {
                new_start,
                new_len: nl,
                old: Vec::new(),
            });
        } else if let Some(removed) = line.strip_prefix('-').filter(|_| !line.starts_with("---"))
            && let Some(h) = v.last_mut()
        {
            h.old.push(removed.to_string());
        }
    }
    v
}

pub fn blame(path: &Path, contents: &str) -> Option<Blame> {
    let dir = path.parent()?;
    let mut child = git(dir)
        .args(["blame", "--porcelain", "--contents", "-", "--"])
        .arg(path.file_name()?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let text = contents.to_string();
    // write on another thread so a large file cannot deadlock on full pipes
    let writer = std::thread::spawn(move || stdin.write_all(text.as_bytes()));
    let out = child.wait_with_output().ok()?;
    let _ = writer.join();
    if out.status.success() {
        return Some(parse_porcelain(&String::from_utf8_lossy(&out.stdout)));
    }
    // a new file git does not track yet: every line is "not committed", like GitLens shows
    let inside = git(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .ok()?;
    (inside.status.success() && String::from_utf8_lossy(&inside.stdout).trim() == "true").then(
        || Blame {
            lines: vec![0; contents.lines().count().max(1)],
            commits: vec![Commit {
                sha: UNCOMMITTED.into(),
                author: "You".into(),
                summary: "Not committed yet".into(),
                ..Default::default()
            }],
        },
    )
}

pub fn parse_porcelain(out: &str) -> Blame {
    let mut b = Blame::default();
    let mut by_sha: HashMap<String, usize> = HashMap::new();
    let mut current: Option<usize> = None;
    for line in out.lines() {
        if let Some(_content) = line.strip_prefix('\t') {
            if let Some(c) = current {
                b.lines.push(c);
            }
            continue;
        }
        let mut words = line.splitn(2, ' ');
        let first = words.next().unwrap_or("");
        let rest = words.next().unwrap_or("");
        if first.len() == 40 && first.bytes().all(|c| c.is_ascii_hexdigit()) {
            let idx = *by_sha.entry(first.to_string()).or_insert_with(|| {
                b.commits.push(Commit {
                    sha: first.to_string(),
                    ..Default::default()
                });
                b.commits.len() - 1
            });
            current = Some(idx);
            continue;
        }
        let Some(c) = current.map(|i| &mut b.commits[i]) else {
            continue;
        };
        match first {
            "author" => c.author = rest.to_string(),
            "author-mail" => c.email = rest.trim_matches(|x| x == '<' || x == '>').to_string(),
            "author-time" => c.time = rest.parse().unwrap_or(0),
            "summary" => c.summary = rest.to_string(),
            _ => {}
        }
    }
    b
}

/// Commits that changed `path` (following renames), newest first.
pub fn history(path: &Path, limit: usize) -> Vec<Commit> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Vec::new();
    };
    let Ok(out) = git(dir)
        .args([
            "log",
            "--follow",
            &format!("-n{limit}"),
            "--format=%H%x1f%an%x1f%ae%x1f%at%x1f%s",
            "--",
        ])
        .arg(name)
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\x1f').collect();
            (f.len() == 5).then(|| Commit {
                sha: f[0].into(),
                author: f[1].into(),
                email: f[2].into(),
                time: f[3].parse().unwrap_or(0),
                summary: f[4].into(),
            })
        })
        .collect()
}

/// Full message, stats and the diff of one commit, limited to `path` when given.
pub fn show(dir: &Path, sha: &str, path: Option<&Path>) -> String {
    let mut c = git(dir);
    c.args([
        "show",
        "--stat",
        "--patch",
        "--format=commit %H%nAuthor: %an <%ae>%nDate:   %ad%n%n%B",
        sha,
    ]);
    if let Some(p) = path {
        // whole history of a renamed file: show the commit's change to it, whatever its name was then
        c.arg("--").arg(p);
    }
    let out = c
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    if out.len() > 400_000 {
        format!(
            "{}\n… (diff cut at 400 KB)",
            &out[..out.floor_char_boundary(400_000)]
        )
    } else {
        out
    }
}

/// The checked-out branch, or `None` outside a repository.
pub fn branch(dir: &Path) -> Option<String> {
    let out = git(dir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !b.is_empty()).then_some(b)
}

/// One changed file: `code` is git's letter (M, A, D, R, ? for untracked), `staged` for the index.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub path: PathBuf,
    pub code: char,
    pub staged: bool,
}

/// What the Git tab shows: uncommitted work, and what this branch changed against its base.
#[derive(Clone, Debug, Default)]
pub struct Changes {
    pub working: Vec<Change>,
    pub branch: Vec<Change>,
    /// The base the branch is compared with, e.g. `origin/develop`.
    pub base: Option<String>,
    /// "#6120 Fix login crash" when `gh` knows a pull request for this branch.
    pub pr: Option<String>,
    pub branches: Vec<String>,
}

/// `git status --porcelain -z`: staged and unstaged changes and untracked files.
pub fn status(root: &Path) -> Vec<Change> {
    let Ok(out) = git(root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.split('\0').filter(|p| !p.is_empty());
    let mut v = Vec::new();
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        let (x, y) = (entry.as_bytes()[0] as char, entry.as_bytes()[1] as char);
        let path = root.join(&entry[3..]);
        if x == 'R' || x == 'C' {
            parts.next(); // the old name of a rename
        }
        if x == '?' {
            v.push(Change {
                path,
                code: '?',
                staged: false,
            });
            continue;
        }
        if x != ' ' {
            v.push(Change {
                path: path.clone(),
                code: x,
                staged: true,
            });
        }
        if y != ' ' {
            v.push(Change {
                path,
                code: y,
                staged: false,
            });
        }
    }
    v
}

/// The branch the current one is compared with: the PR's base when `gh` answers, else
/// `origin/HEAD`, else the first of origin/develop, origin/main, origin/master that exists.
pub fn base_branch(root: &Path) -> (Option<String>, Option<String>) {
    if let Ok(out) = Command::new("gh")
        .args(["pr", "view", "--json", "number,title,baseRefName"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        && out.status.success()
        && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout)
        && let Some(base) = v["baseRefName"].as_str()
    {
        let pr = format!("#{} {}", v["number"], v["title"].as_str().unwrap_or(""));
        return (Some(format!("origin/{base}")), Some(pr));
    }
    let ok = |r: &str| {
        git(root)
            .args(["rev-parse", "--verify", "--quiet", r])
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if let Ok(out) = git(root)
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .output()
        && out.status.success()
    {
        let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !b.is_empty() {
            return (Some(b), None);
        }
    }
    (
        ["origin/develop", "origin/main", "origin/master"]
            .into_iter()
            .find(|b| ok(b))
            .map(String::from),
        None,
    )
}

/// Files this branch changed since it left `base` (what a pull request shows).
pub fn branch_changes(root: &Path, base: &str) -> Vec<Change> {
    let Ok(out) = git(root)
        .args(["diff", "--name-status", "-M", &format!("{base}...HEAD")])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let code = f.next()?.chars().next()?;
            let path = f.next_back()?; // the new name for renames
            Some(Change {
                path: root.join(path),
                code,
                staged: false,
            })
        })
        .collect()
}

pub fn branches(root: &Path) -> Vec<String> {
    git(root)
        .args(["branch", "--format=%(refname:short)"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

pub fn changes(root: &Path) -> Changes {
    let (base, pr) = base_branch(root);
    let branch = base
        .as_deref()
        .map(|b| branch_changes(root, b))
        .unwrap_or_default();
    Changes {
        working: status(root),
        branch,
        base,
        pr,
        branches: branches(root),
    }
}

/// The diff of one file: against `base...HEAD` for branch changes, else the uncommitted change.
pub fn file_diff(root: &Path, path: &Path, base: Option<&str>, staged: bool) -> String {
    let mut c = git(root);
    c.arg("diff");
    match base {
        Some(b) => {
            c.arg(format!("{b}...HEAD"));
        }
        None if staged => {
            c.arg("--cached");
        }
        None => {}
    }
    c.arg("--").arg(path);
    let out = c
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    if out.is_empty() && base.is_none() {
        // untracked: the whole file is new
        return std::fs::read_to_string(path)
            .map(|t| t.lines().map(|l| format!("+{l}\n")).collect())
            .unwrap_or_default();
    }
    out
}

/// Runs a git command that changes the repository (stage, unstage, commit, checkout).
/// Returns git's error text when it fails.
pub fn run(root: &Path, args: &[&str]) -> Result<(), String> {
    let out = git(root)
        .args(args)
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// "just now", "5 minutes ago", "3 days ago", "2 years ago".
pub fn ago(time: i64, now: i64) -> String {
    let s = (now - time).max(0);
    let (n, unit) = match s {
        0..60 => return "just now".into(),
        60..3600 => (s / 60, "minute"),
        3600..86_400 => (s / 3600, "hour"),
        86_400..2_592_000 => (s / 86_400, "day"),
        2_592_000..31_536_000 => (s / 2_592_000, "month"),
        _ => (s / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times() {
        assert_eq!(ago(100, 130), "just now");
        assert_eq!(ago(0, 60), "1 minute ago");
        assert_eq!(ago(0, 3 * 86_400), "3 days ago");
        assert_eq!(ago(0, 2 * 31_536_000), "2 years ago");
    }

    #[test]
    fn parses_porcelain_with_repeated_commits() {
        let a = "a".repeat(40);
        let out = format!(
            "{a} 1 1 2\nauthor Ana\nauthor-mail <ana@x.io>\nauthor-time 1700000000\nsummary First\nfilename A.kt\n\tline one\n{a} 2 2\n\tline two\n{UNCOMMITTED} 3 3 1\nauthor Not Committed Yet\nauthor-time 1700000100\nsummary Version of A.kt from -\nfilename A.kt\n\tline three\n"
        );
        let b = parse_porcelain(&out);
        assert_eq!(b.lines, vec![0, 0, 1]);
        assert_eq!(b.line(0).unwrap().author, "Ana");
        assert_eq!(b.line(0).unwrap().email, "ana@x.io");
        assert!(b.line(2).unwrap().is_uncommitted());
    }

    #[test]
    fn blames_a_real_repo_with_unsaved_edits() {
        let dir = std::env::temp_dir().join(format!("droid-lite-git-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "{args:?}"
            )
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "Test Author"]);
        run(&["config", "user.email", "test@example.com"]);
        std::fs::write(dir.join("A.kt"), "fun a() = 1\nfun b() = 2\n").unwrap();
        run(&["add", "A.kt"]);
        run(&["commit", "-q", "-m", "Add A"]);
        let path = dir.join("A.kt");
        let b = blame(&path, "fun a() = 1\nfun new() = 0\nfun b() = 2\n").unwrap();
        assert_eq!(b.lines.len(), 3);
        assert_eq!(b.line(0).unwrap().summary, "Add A");
        assert!(b.line(1).unwrap().is_uncommitted());
        assert_eq!(b.line(2).unwrap().author, "Test Author");
        let h = history(&path, 10);
        assert_eq!(h.len(), 1);
        assert!(show(&dir, &h[0].sha, Some(&path)).contains("+fun b() = 2"));
        let new = blame(&dir.join("Untracked.kt"), "x\ny").unwrap();
        assert_eq!(new.lines.len(), 2);
        assert!(
            new.line(1).unwrap().is_uncommitted(),
            "an untracked file is all uncommitted"
        );
        assert!(
            blame(
                &std::env::temp_dir().join("not-a-repo-droid-lite/A.kt"),
                "x"
            )
            .is_none()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn status_branch_changes_stage_and_commit() {
        let dir =
            std::env::temp_dir().join(format!("droid-lite-git-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let g = |args: &[&str]| run(&dir, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.name", "T"]);
        g(&["config", "user.email", "t@example.com"]);
        std::fs::write(dir.join("A.kt"), "a\n").unwrap();
        std::fs::write(dir.join("B.kt"), "b\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "base"]);
        g(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("A.kt"), "a2\n").unwrap();
        g(&["commit", "-q", "-am", "change A"]);
        // uncommitted: B modified and staged, C new
        std::fs::write(dir.join("B.kt"), "b2\n").unwrap();
        g(&["add", "B.kt"]);
        std::fs::write(dir.join("C kt.kt"), "c\n").unwrap();
        let st = status(&dir);
        assert!(
            st.contains(&Change {
                path: dir.join("B.kt"),
                code: 'M',
                staged: true
            }),
            "{st:?}"
        );
        assert!(
            st.contains(&Change {
                path: dir.join("C kt.kt"),
                code: '?',
                staged: false
            }),
            "{st:?}"
        );
        let br = branch_changes(&dir, "main");
        assert_eq!(
            br,
            vec![Change {
                path: dir.join("A.kt"),
                code: 'M',
                staged: false
            }]
        );
        assert!(file_diff(&dir, &dir.join("A.kt"), Some("main"), false).contains("+a2"));
        assert!(file_diff(&dir, &dir.join("B.kt"), None, true).contains("+b2"));
        assert_eq!(file_diff(&dir, &dir.join("C kt.kt"), None, false), "+c\n");
        assert_eq!(branches(&dir), vec!["feature", "main"]);
        g(&["commit", "-q", "-m", "B"]);
        assert!(status(&dir).iter().all(|c| c.code == '?'));
        assert!(run(&dir, &["checkout", "nope"]).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hunks_of_added_modified_and_deleted_lines() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nB\nc\nnew\nd\n";
        let h = hunks(old, new);
        assert_eq!(
            h,
            vec![
                Hunk {
                    new_start: 1,
                    new_len: 1,
                    old: vec!["b".into()]
                },
                Hunk {
                    new_start: 3,
                    new_len: 1,
                    old: vec![]
                },
                Hunk {
                    new_start: 5,
                    new_len: 0,
                    old: vec!["e".into()]
                },
            ]
        );
        assert_eq!(h.iter().map(Hunk::kind).collect::<String>(), "MAD");
        assert!(hunks(old, old).is_empty());
    }
}
