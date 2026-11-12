//! GitLens-style data from the `git` command: line blame (of the editor text, unsaved edits
//! included), file history and one commit's diff.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
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
        .stderr(Stdio::null());
    c
}

/// Blame of `contents` (the editor text) for `path`. `None` when the file is not tracked by git.
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
    out.status
        .success()
        .then(|| parse_porcelain(&String::from_utf8_lossy(&out.stdout)))
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
        assert!(blame(&dir.join("Untracked.kt"), "x").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
