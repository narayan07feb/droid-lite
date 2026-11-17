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
    file_at(path, "HEAD")
}

/// The file's text at commit `rev`; None when it did not exist there.
pub fn file_at(path: &Path, rev: &str) -> Option<String> {
    let (dir, name) = (path.parent()?, path.file_name()?);
    let out = git(dir)
        .arg("show")
        .arg(format!("{rev}:./{}", name.to_string_lossy()))
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `git diff --no-index` of two texts, with `args` before the paths; why it could not run. The
/// texts go to new files (0600) in a new private folder (0700): a name planted in the temp folder
/// (a symlink) is never followed. A diff tool the repo or the environment sets
/// (`diff.external`, `GIT_EXTERNAL_DIFF`) never runs.
fn diff_no_index(old: &str, new: &str, args: &[&str]) -> Result<String, String> {
    let dir = private_temp_dir("droid-lite-hunks").map_err(|e| e.to_string())?;
    let (a, b) = (dir.join("old"), dir.join("new"));
    let out = new_private_file(&a, old)
        .and_then(|()| new_private_file(&b, new))
        .map_err(|e| e.to_string())
        .and_then(|()| {
            no_index_cmd(&a, &b, args)
                .output()
                .map_err(|e| format!("git did not start: {e}"))
        });
    let _ = std::fs::remove_dir_all(&dir);
    let out = out?;
    // 0: the same, 1: different; anything else is an error
    match out.status.code() {
        Some(0 | 1) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        _ => Err(match String::from_utf8_lossy(&out.stderr).trim() {
            "" => format!("git diff failed ({})", out.status),
            e => format!("git diff failed: {e}"),
        }),
    }
}

fn no_index_cmd(a: &Path, b: &Path, args: &[&str]) -> Command {
    let mut c = Command::new("git");
    c.args(["diff", "--no-index", "--no-color", "--no-ext-diff"])
        .args(args)
        .arg("--")
        .arg(a)
        .arg(b)
        .stdin(Stdio::null());
    c
}

/// A new folder only this user can open, under the temp folder; never one that was there.
fn private_temp_dir(prefix: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    loop {
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            r => return r.map(|()| dir),
        }
    }
}

/// Writes `text` to a new file `path` (0600); fails when something is there.
fn new_private_file(path: &Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(text.as_bytes())
}

/// The `-U0` diff from `old` to `new` as lines, for side-by-side views: a `\r` at a line end
/// does not count as a change, text that looks binary is still compared line by line, and the
/// "\\ No newline at end of file" notes are left out. Why git could not compare them.
pub fn line_diff(old: &str, new: &str) -> Result<Vec<String>, String> {
    if old == new {
        return Ok(Vec::new());
    }
    let args = ["-U0", "--text", "--ignore-cr-at-eol"];
    Ok(diff_no_index(old, new, &args)?
        .lines()
        .filter(|l| !l.starts_with('\\'))
        .map(String::from)
        .collect())
}

/// Line changes from `old` to `new`, through `git diff --no-index -U0`.
pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    if old == new {
        return Vec::new();
    }
    // a git that fails marks nothing (the gutter shows no change bars)
    let out = diff_no_index(old, new, &["-U0"]).unwrap_or_default();
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

/// The commit HEAD points at, or `None` outside a repository or before the first commit.
pub fn head_sha(dir: &Path) -> Option<String> {
    let out = git(dir).args(["rev-parse", "HEAD"]).output().ok()?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !sha.is_empty()).then_some(sha)
}

/// One changed file: `code` is git's letter (M, A, D, R, ? for untracked), `staged` for the index.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub path: PathBuf,
    pub code: char,
    pub staged: bool,
}

/// What the Current changes tab shows: uncommitted work, and what this branch changed against its base.
#[derive(Clone, Debug, Default)]
pub struct Changes {
    /// `git status` worked: the project is in a git repository.
    pub repo: bool,
    pub working: Vec<Change>,
    pub branch: Vec<Change>,
    /// The base the branch is compared with, e.g. `origin/develop`.
    pub base: Option<String>,
    /// "#6120 Fix login crash" when `gh` knows a pull request for this branch.
    pub pr: Option<String>,
    /// The base found without a choice (the PR's base, else origin/HEAD, develop, main, master).
    pub auto_base: Option<String>,
    /// The user's base choice could not be found (an unknown PR number or branch).
    pub base_error: Option<String>,
    pub branches: Vec<String>,
}

/// An open pull request: number, title and its branch.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenPr {
    pub number: u64,
    pub title: String,
    pub head: String,
}

/// `gh pr list --json number,title,headRefName` output.
pub fn parse_pr_list(json: &[u8]) -> Vec<OpenPr> {
    serde_json::from_slice::<Vec<serde_json::Value>>(json)
        .unwrap_or_default()
        .iter()
        .filter_map(|v| {
            Some(OpenPr {
                number: v["number"].as_u64()?,
                title: v["title"].as_str().unwrap_or("").to_string(),
                head: v["headRefName"].as_str()?.to_string(),
            })
        })
        .collect()
}

/// Open pull requests of the repository (newest first), for the base PR picker.
pub fn open_prs(root: &Path) -> Vec<OpenPr> {
    Command::new(gh_program())
        .args([
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            "100",
            "--json",
            "number,title,headRefName",
        ])
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| parse_pr_list(&o.stdout))
        .unwrap_or_default()
}

/// Which pull requests the sidebar lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrFilter {
    /// Opened by you.
    Mine,
    /// Waiting for your review.
    Review,
    /// Yours and approved: ready to merge.
    Approved,
}

impl PrFilter {
    pub fn label(self) -> &'static str {
        match self {
            PrFilter::Mine => "Your PRs",
            PrFilter::Review => "Awaiting your review",
            PrFilter::Approved => "Approved",
        }
    }
}

/// One pull request in the sidebar list.
#[derive(Clone, Debug, PartialEq)]
pub struct PrItem {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub draft: bool,
    pub url: String,
    /// APPROVED, CHANGES_REQUESTED, REVIEW_REQUIRED or empty.
    pub review: String,
    /// Reviewers whose latest review approves it.
    pub approvals: u32,
    /// The PR's branch (`headRefName`).
    pub head: String,
    /// The PR's last commit (`headRefOid`): a fork's `main` is not this repository's `main`.
    pub head_oid: String,
}

/// `gh pr list --json number,title,author,isDraft,url,reviewDecision,latestReviews,headRefName,headRefOid` output.
/// GitHub leaves `reviewDecision` empty when the branch rules ask for no review: then the
/// reviewers' latest reviews decide (any change request wins over approvals).
pub fn parse_pr_items(json: &[u8]) -> Vec<PrItem> {
    serde_json::from_slice::<Vec<serde_json::Value>>(json)
        .unwrap_or_default()
        .iter()
        .filter_map(|v| {
            let latest = v["latestReviews"].as_array().cloned().unwrap_or_default();
            let count = |state: &str| latest.iter().filter(|r| r["state"] == state).count() as u32;
            let approvals = count("APPROVED");
            let decision = v["reviewDecision"].as_str().unwrap_or("");
            let review = if !decision.is_empty() {
                decision
            } else if count("CHANGES_REQUESTED") > 0 {
                "CHANGES_REQUESTED"
            } else if approvals > 0 {
                "APPROVED"
            } else {
                ""
            };
            Some(PrItem {
                approvals,
                number: v["number"].as_u64()?,
                title: v["title"].as_str().unwrap_or("").to_string(),
                author: v["author"]["login"].as_str().unwrap_or("").to_string(),
                draft: v["isDraft"].as_bool().unwrap_or(false),
                url: v["url"].as_str().unwrap_or("").to_string(),
                review: review.to_string(),
                head: v["headRefName"].as_str().unwrap_or("").to_string(),
                head_oid: v["headRefOid"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// The `gh` program: `DROID_LITE_GH` (tests point it at a fake), else `gh` from the PATH.
pub fn gh_program() -> std::ffi::OsString {
    std::env::var_os("DROID_LITE_GH").unwrap_or_else(|| "gh".into())
}

/// Why `gh` could not start: not installed is said plainly.
fn gh_spawn_error(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        "The GitHub CLI (gh) is not installed: brew install gh, then gh auth login".into()
    } else {
        format!("gh: {e}")
    }
}

/// Why `gh` failed: the sign-in hint, else its first error line, past git's fetch progress
/// ("From github.com:…").
fn gh_failure(out: &std::process::Output) -> String {
    let e = String::from_utf8_lossy(&out.stderr);
    // gh exits with 4 when it needs a sign-in
    if out.status.code() == Some(4) || e.contains("gh auth login") {
        return "Not signed in to GitHub: run gh auth login in a terminal".into();
    }
    let words = "error|fatal|failed|could not|not found|denied";
    let lines = || e.lines().map(str::trim).filter(|l| !l.is_empty());
    let line = lines()
        .find(|l| {
            let l = l.to_lowercase();
            words.split('|').any(|w| l.contains(w))
        })
        .or_else(|| lines().next())
        .unwrap_or("gh failed");
    match line.char_indices().nth(MAX_ERROR_CHARS) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}

/// The longest error line shown; gh's stdout and stderr are kept up to [`MAX_GH_OUTPUT`] bytes.
const MAX_ERROR_CHARS: usize = 300;
const MAX_GH_OUTPUT: usize = 64 * 1024;

/// How long `gh pr checkout` may take (a fetch over a slow network) before it is stopped.
pub const CHECKOUT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// How long `gh pr diff --name-only` (the files a checkout would write) may take.
const DIFF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// How long a stopped gh (and the git it started) gets after SIGTERM, to remove `index.lock`,
/// before SIGKILL.
pub const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Stops a running [`checkout_pr`] from another thread, and knows its gh process group.
#[derive(Default)]
pub struct Ctl {
    stop: std::sync::atomic::AtomicBool,
    pgid: std::sync::atomic::AtomicI32,
}

impl Ctl {
    /// Asks the checkout to stop: gh gets SIGTERM, SIGKILL after [`STOP_GRACE`]; gh never starts
    /// when it has not started yet.
    pub fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    fn stopped(&self) -> bool {
        self.stop.load(std::sync::atomic::Ordering::SeqCst)
    }
    /// The process group of the running gh, 0 when none runs.
    #[cfg(test)]
    pub fn pgid(&self) -> i32 {
        self.pgid.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Why [`output_within`] gave no output.
enum Ended {
    Spawn(std::io::Error),
    TimedOut,
    Stopped,
}

/// Runs `gh` in `root` (off the UI thread: it uses the network); its stdout, or why it failed.
fn gh(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new(gh_program())
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .map_err(gh_spawn_error)?;
    if !out.status.success() {
        return Err(gh_failure(&out));
    }
    Ok(out.stdout)
}

/// The open pull requests for `filter`, newest first.
pub fn list_prs(root: &Path, filter: PrFilter) -> Result<Vec<PrItem>, String> {
    let who = match filter {
        PrFilter::Mine => ["--author", "@me"],
        PrFilter::Review => ["--search", "review-requested:@me"],
        PrFilter::Approved => ["--author", "@me"],
    };
    let mut args = vec!["pr", "list", "--state", "open", "--limit", "100"];
    args.extend(who);
    args.extend([
        "--json",
        "number,title,author,isDraft,url,reviewDecision,latestReviews,headRefName,headRefOid",
    ]);
    let prs = gh(root, &args).map(|o| parse_pr_items(&o))?;
    // decided here, not by GitHub's search, so it matches the ✓ in the other lists
    Ok(match filter {
        PrFilter::Approved => prs.into_iter().filter(|p| p.review == "APPROVED").collect(),
        _ => prs,
    })
}

/// The repository operation that is half done (a rebase stopped at a conflict or an `edit`, a
/// merge with conflicts, a `git am`, a cherry-pick or revert of several commits, a bisect), from
/// the files git keeps while it runs.
pub fn operation_in_progress(root: &Path) -> Option<&'static str> {
    [
        ("rebase-merge", "rebase"),
        // `git am` keeps its state in rebase-apply too, with this file in it
        ("rebase-apply/applying", "git am"),
        ("rebase-apply", "rebase"),
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        // the commits still to go of `git cherry-pick A B` or `git revert A B`
        ("sequencer", "cherry-pick or revert"),
        ("BISECT_LOG", "bisect"),
    ]
    .into_iter()
    .find(|(file, _)| {
        git(root)
            .args(["rev-parse", "--git-path", file])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .is_some_and(|o| {
                let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
                !p.is_empty() && root.join(p).exists()
            })
    })
    .map(|(_, name)| name)
}

/// A detached HEAD with commits that no branch (local or remote) or tag contains: a checkout
/// would leave them reachable only from the reflog.
fn head_on_no_branch(root: &Path) -> bool {
    let on_branch = git(root)
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .is_ok_and(|o| o.status.success());
    if on_branch {
        return false;
    }
    git(root)
        .args([
            "for-each-ref",
            "--count=1",
            "--contains",
            "HEAD",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ])
        .output()
        .is_ok_and(|o| o.status.success() && o.stdout.is_empty())
}

/// `head` and up to 10 of `paths` (relative to `root`: `../x` for one beside it), one per
/// line, then how many more.
pub fn file_list<'a>(head: &str, root: &Path, paths: impl IntoIterator<Item = &'a Path>) -> String {
    let paths: Vec<&Path> = paths.into_iter().collect();
    let mut msg = head.to_string();
    for p in paths.iter().take(10) {
        let rel = p.strip_prefix(root).unwrap_or(p);
        msg.push_str(&format!("\n{}", rel.display()));
    }
    if paths.len() > 10 {
        msg.push_str(&format!("\n… and {} more", paths.len() - 10));
    }
    msg
}

/// Where `root` is inside its repository (`git rev-parse --show-prefix`): "" at the top,
/// `android/` for a project in that folder.
fn repo_prefix(root: &Path) -> String {
    git(root)
        .args(["rev-parse", "--show-prefix"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// A path git gives relative to the repository's top (status, diff, gh) as a path under `root`
/// (`prefix` from [`repo_prefix`]); one outside the project becomes `root/../…`.
fn repo_path(root: &Path, prefix: &str, p: &str) -> PathBuf {
    match p.strip_prefix(prefix) {
        Some(rest) => root.join(rest),
        None => {
            let mut out = root.to_path_buf();
            for _ in prefix.split('/').filter(|c| !c.is_empty()) {
                out.push("..");
            }
            out.join(p)
        }
    }
}

/// The ignored files on disk that pull request `number` changes (`gh pr diff --name-only`):
/// a checkout overwrites ignored files without a word. `Ok(None)` when gh cannot say.
fn ignored_overwrites(
    root: &Path,
    number: u64,
    gh: &std::ffi::OsStr,
    ctl: &Ctl,
) -> Result<Option<Vec<PathBuf>>, String> {
    let mut cmd = Command::new(gh);
    cmd.args(["pr", "diff", &number.to_string(), "--name-only"])
        .current_dir(root)
        .env("GH_PROMPT_DISABLED", "1");
    let out = match output_within(cmd, DIFF_TIMEOUT, ctl) {
        Ok(o) if o.status.success() => o,
        Err(Ended::Stopped) => return Err(stopped(number)),
        _ => return Ok(None),
    };
    let prefix = repo_prefix(root);
    let on_disk: Vec<PathBuf> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| repo_path(root, &prefix, l))
        .filter(|p| p.is_file())
        .collect();
    let mut ignored = Vec::new();
    for chunk in on_disk.chunks(500) {
        let Ok(o) = git(root)
            .args([
                "ls-files",
                "-z",
                "-o",
                "-i",
                "--exclude-standard",
                "--full-name",
                "--",
            ])
            .args(chunk)
            .output()
        else {
            return Ok(None);
        };
        ignored.extend(
            String::from_utf8_lossy(&o.stdout)
                .split('\0')
                .filter(|p| !p.is_empty())
                .map(|p| repo_path(root, &prefix, p)),
        );
    }
    Ok(Some(ignored))
}

fn stopped(number: u64) -> String {
    format!("Check out of #{number} was stopped")
}

/// Checks out pull request `number` with `<gh> pr checkout` (`gh` is the program, a fake in
/// tests), stopped after `timeout` or by `ctl`, and returns the branch it is on now and whether a
/// submodule now points at another commit than its checkout (`git submodule update` is due).
/// Never loses tracked or unsaved work: it refuses when git status fails, during a rebase, merge,
/// `git am`, cherry-pick, revert or bisect, on a detached HEAD whose commits no branch or tag has,
/// when tracked files have uncommitted changes (it lists them; untracked files, also inside
/// submodules, do not count), and when the PR changes ignored files that are on disk (when
/// `gh pr diff` cannot list the PR's files, that check is skipped).
pub fn checkout_pr(
    root: &Path,
    number: u64,
    gh: &std::ffi::OsStr,
    timeout: std::time::Duration,
    ctl: &Ctl,
) -> Result<(String, bool), String> {
    let changes = status_with(root, &["--ignore-submodules=untracked"])?;
    if let Some(op) = operation_in_progress(root) {
        return Err(format!("Finish or abort the {op} first"));
    }
    if head_on_no_branch(root) {
        return Err("HEAD has commits on no branch: create a branch first".into());
    }
    let mut dirty: Vec<&Path> = Vec::new();
    for c in changes.iter().filter(|c| c.code != '?') {
        if !dirty.contains(&c.path.as_path()) {
            dirty.push(&c.path);
        }
    }
    if !dirty.is_empty() {
        return Err(file_list(
            "Commit or stash your changes first:",
            root,
            dirty,
        ));
    }
    if let Some(ignored) = ignored_overwrites(root, number, gh, ctl)?
        && !ignored.is_empty()
    {
        return Err(file_list(
            "Move or back up these ignored files first:",
            root,
            ignored.iter().map(PathBuf::as_path),
        ));
    }
    let mut cmd = Command::new(gh);
    cmd.args(["pr", "checkout", &number.to_string()])
        .current_dir(root)
        // never wait for a password or a question nobody sees
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1");
    let out = output_within(cmd, timeout, ctl).map_err(|e| match e {
        Ended::Spawn(e) => gh_spawn_error(e),
        Ended::TimedOut => {
            "Check out timed out; the working tree may be part-switched: run git status".into()
        }
        Ended::Stopped => stopped(number),
    })?;
    if !out.status.success() {
        return Err(gh_failure(&out));
    }
    let branch = branch(root).ok_or("Checked out, but git shows no branch")?;
    let submodules = git(root)
        .args(["submodule", "status"])
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.starts_with('+'))
        });
    Ok((branch, submodules))
}

/// Runs `cmd` in a process group of its own (its id in `ctl` while it runs), its stdout and
/// stderr kept up to [`MAX_GH_OUTPUT`] bytes each. After `timeout`, or when `ctl` is stopped, the
/// whole group (git that gh started too) gets SIGTERM, so git can remove its `index.lock`, and
/// SIGKILL after [`STOP_GRACE`].
fn output_within(
    mut cmd: Command,
    timeout: std::time::Duration,
    ctl: &Ctl,
) -> Result<std::process::Output, Ended> {
    use std::io::Read;
    if ctl.stopped() {
        return Err(Ended::Stopped);
    }
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(Ended::Spawn)?;
    let pgid = child.id() as i32;
    ctl.pgid.store(pgid, std::sync::atomic::Ordering::SeqCst);
    // each pipe is read to its end on a thread, so the program never blocks on a full pipe
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            if let Some(mut pipe) = pipe {
                let mut buf = [0u8; 8192];
                while let Ok(n @ 1..) = pipe.read(&mut buf) {
                    let room = MAX_GH_OUTPUT.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
            let _ = tx.send(kept);
        });
        rx
    };
    let stdout = read(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = read(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = std::time::Instant::now() + timeout;
    let tick = || std::thread::sleep(std::time::Duration::from_millis(20));
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if std::time::Instant::now() < deadline && !ctl.stopped() => tick(),
            _ => {
                let why = if ctl.stopped() {
                    Ended::Stopped
                } else {
                    Ended::TimedOut
                };
                end_group(&mut child, pgid, STOP_GRACE);
                ctl.pgid.store(0, std::sync::atomic::Ordering::SeqCst);
                return Err(why);
            }
        }
    };
    ctl.pgid.store(0, std::sync::atomic::Ordering::SeqCst);
    // a tool it left running may hold the pipes open: wait no longer than the time left
    let rest = |rx: std::sync::mpsc::Receiver<Vec<u8>>| {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        rx.recv_timeout(left.max(std::time::Duration::from_secs(1)))
            .unwrap_or_default()
    };
    Ok(std::process::Output {
        status,
        stdout: rest(stdout),
        stderr: rest(stderr),
    })
}

/// SIGTERM to the group `child` leads, then SIGKILL to what is left of it after `grace`,
/// again until the group is empty (a process forked while the signal went out escapes one
/// SIGKILL), for [`KILL_LIMIT`] at most; `child` is reaped. Only then is the checkout stopped.
fn end_group(child: &mut std::process::Child, pgid: i32, grace: std::time::Duration) {
    #[cfg(unix)]
    {
        // SAFETY: kill(2) on the group this child leads; it has not been reaped yet, so the id
        // is still this group's
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
        let until = std::time::Instant::now() + grace;
        let mut reaped = false;
        while std::time::Instant::now() < until {
            reaped = reaped || child.try_wait().is_ok_and(|s| s.is_some());
            // SAFETY: signal 0 only checks whether the group still has a process
            if reaped && unsafe { libc::kill(-pgid, 0) } != 0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // SAFETY: as above; a group id stays taken while any process of the group lives
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
        // the leader is reaped first: its zombie would keep the group alive
        let _ = child.wait();
        let until = std::time::Instant::now() + KILL_LIMIT;
        // SAFETY: as above; signal 0 only checks whether the group still has a process
        while unsafe { libc::kill(-pgid, 0) } == 0 && std::time::Instant::now() < until {
            // SAFETY: as above
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// How long [`end_group`] goes on killing a group that is not empty yet.
const KILL_LIMIT: std::time::Duration = std::time::Duration::from_secs(1);

/// A pull request's whole diff (`gh pr diff`), split into one diff per file.
pub fn pr_diff(root: &Path, number: u64) -> Result<Vec<(String, String)>, String> {
    let n = number.to_string();
    gh(root, &["pr", "diff", &n, "--color", "never"])
        .map(|o| split_diff(&String::from_utf8_lossy(&o)))
}

/// (path, its diff) per `diff --git a/… b/…` section; the path is the new name.
pub fn split_diff(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for l in text.split_inclusive('\n') {
        if let Some(rest) = l.strip_prefix("diff --git ") {
            let rest = rest.trim_end();
            let b = rest.rsplit_once(" b/").map_or(rest, |(_, b)| b);
            out.push((b.to_string(), String::new()));
        }
        if let Some((_, d)) = out.last_mut() {
            d.push_str(l);
        }
    }
    out
}

/// `#123` or `123` as a PR number.
pub fn pr_number(choice: &str) -> Option<u64> {
    choice.trim().trim_start_matches('#').parse().ok()
}

fn ref_exists(root: &Path, r: &str) -> bool {
    git(root)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{r}^{{commit}}"),
        ])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The git ref for a base choice: a PR number becomes `origin/<its branch>` (fetched when it is
/// not here yet); a branch name is used as it is, or as `origin/<name>`.
pub fn resolve_base(root: &Path, choice: &str) -> Result<String, String> {
    let branch = match pr_number(choice) {
        Some(n) => {
            let out = Command::new(gh_program())
                .args(["pr", "view", &n.to_string(), "--json", "headRefName"])
                .current_dir(root)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("gh: {e}"))?;
            let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
            let head = v["headRefName"]
                .as_str()
                .ok_or_else(|| format!("PR #{n} not found"))?;
            format!("origin/{head}")
        }
        None => choice.trim().to_string(),
    };
    if ref_exists(root, &branch) {
        return Ok(branch);
    }
    let remote = format!("origin/{branch}");
    if !branch.starts_with("origin/") && ref_exists(root, &remote) {
        return Ok(remote);
    }
    // a PR's branch that was never fetched: fetch only that branch
    if let Some(name) = branch.strip_prefix("origin/") {
        let _ = git(root)
            .args([
                "fetch",
                "--quiet",
                "origin",
                &format!("refs/heads/{name}:refs/remotes/origin/{name}"),
            ])
            .output();
        if ref_exists(root, &branch) {
            return Ok(branch);
        }
    }
    Err(format!("{} not found", choice.trim()))
}

/// `git status --porcelain -z`: staged and unstaged changes and untracked files, or why git
/// status failed (not a repository, a broken index).
pub fn status(root: &Path) -> Result<Vec<Change>, String> {
    status_with(root, &[])
}

/// [`status`] with more `git status` options (`--ignore-submodules=untracked`).
fn status_with(root: &Path, extra: &[&str]) -> Result<Vec<Change>, String> {
    let out = git(root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .args(extra)
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("git status: {e}"))?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        let line = e.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(format!("git status failed: {}", line.trim()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // porcelain paths start at the repository's top, which may be above the project
    let prefix = repo_prefix(root);
    let mut parts = text.split('\0').filter(|p| !p.is_empty());
    let mut v = Vec::new();
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        let (x, y) = (entry.as_bytes()[0] as char, entry.as_bytes()[1] as char);
        let path = repo_path(root, &prefix, &entry[3..]);
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
    Ok(v)
}

/// The branch the current one is compared with: the PR's base when `gh` answers, else
/// `origin/HEAD`, else the first of origin/develop, origin/main, origin/master that exists.
pub fn base_branch(root: &Path) -> (Option<String>, Option<String>) {
    if let Ok(out) = Command::new(gh_program())
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
            .find(|b| ref_exists(root, b))
            .map(String::from),
        None,
    )
}

/// Files this branch changed since it left `base` (what a pull request shows).
pub fn branch_changes(root: &Path, base: &str) -> Vec<Change> {
    let Ok(out) = git(root)
        .args(["diff", "--no-ext-diff", "--name-status", "-M", &format!("{base}...HEAD")])
        .output()
    else {
        return Vec::new();
    };
    let prefix = repo_prefix(root);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let code = f.next()?.chars().next()?;
            let path = f.next_back()?; // the new name for renames
            Some(Change {
                path: repo_path(root, &prefix, path),
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

/// `choice`: the base the user picked for this branch (`#123` or a branch), else the auto base.
pub fn changes(root: &Path, choice: Option<&str>) -> Changes {
    let (auto_base, pr) = base_branch(root);
    let (base, base_error) = match choice.map(|c| resolve_base(root, c)) {
        Some(Ok(b)) => (Some(b), None),
        Some(Err(e)) => (auto_base.clone(), Some(e)),
        None => (auto_base.clone(), None),
    };
    let branch = base
        .as_deref()
        .map(|b| branch_changes(root, b))
        .unwrap_or_default();
    let working = status(root);
    Changes {
        repo: working.is_ok(),
        working: working.unwrap_or_default(),
        branch,
        base,
        pr,
        auto_base,
        base_error,
        branches: branches(root),
    }
}

/// The diff of one file: against `base...HEAD` for branch changes, else the uncommitted change.
pub fn file_diff(root: &Path, path: &Path, base: Option<&str>, staged: bool) -> String {
    let mut c = git(root);
    c.args(["diff", "--no-ext-diff"]);
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

/// Puts `path` back as it was at `source` (`HEAD`, or where the branch left its base), in the
/// index and the working tree. A file `source` does not have is removed.
pub fn revert_file(root: &Path, path: &Path, source: &str) -> Result<(), String> {
    let p = path.to_string_lossy();
    run(
        root,
        &[
            "restore",
            &format!("--source={source}"),
            "--staged",
            "--worktree",
            "--",
            &p,
        ],
    )
}

/// The commit where this branch left `base`.
pub fn merge_base(root: &Path, base: &str) -> Option<String> {
    let out = git(root).args(["merge-base", base, "HEAD"]).output().ok()?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !sha.is_empty()).then_some(sha)
}

/// A file of the work tree and one commit of its repository: the repository's top folder, the
/// file's path in it and the commit's full sha.
#[derive(Clone, Debug, PartialEq)]
pub struct RevFile {
    pub top: PathBuf,
    pub rel: String,
    pub sha: String,
}

/// git's stdout (trimmed), or its error text.
fn out_or_err(c: &mut Command) -> Result<String, String> {
    let out = c
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| e.to_string())?;
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    match out.status.success() {
        true => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        false if err.is_empty() => Err(format!("git failed ({})", out.status)),
        false => Err(err),
    }
}

/// `path` at commit `rev`: its repository and the commit, or why there is none ("not a git
/// repository", "no commit yet", git's error).
pub fn rev_file(path: &Path, rev: &str) -> Result<RevFile, String> {
    let (dir, name) = path
        .parent()
        .zip(path.file_name())
        .ok_or("not a file path")?;
    let top = out_or_err(git(dir).args(["rev-parse", "--show-toplevel"]))
        .map_err(|_| "not a git repository".to_string())?;
    let prefix = out_or_err(git(dir).args(["rev-parse", "--show-prefix"]))?;
    let commit = format!("{rev}^{{commit}}");
    let sha = out_or_err(git(dir).args(["rev-parse", "--verify", "--quiet", &commit])).map_err(
        |_| match rev {
            "HEAD" => "no commit yet".to_string(),
            _ => format!("{rev} is not a commit here"),
        },
    )?;
    Ok(RevFile {
        top: PathBuf::from(top),
        rel: format!("{prefix}{}", name.to_string_lossy()),
        sha,
    })
}

/// The text of `rel` at commit `sha` of repository `top`: `Ok(None)` when that commit does not
/// have it; an error over `max` bytes (the blob is then not read) or when git fails.
pub fn blob_at(top: &Path, sha: &str, rel: &str, max: usize) -> Result<Option<String>, String> {
    let ls = out_or_err(git(top).args(["ls-tree", "-l", sha, "--", rel]))?;
    // <mode> <type> <sha> <size>\t<path>
    let mut f = ls.split_whitespace();
    let (kind, size) = (f.nth(1), f.nth(1).and_then(|s| s.parse::<usize>().ok()));
    if kind != Some("blob") {
        return Ok(None);
    }
    if size.is_some_and(|s| s > max) {
        return Err(format!("the file is over {} KB", max / 1024));
    }
    let out = git(top)
        .args(["cat-file", "blob", &format!("{sha}:{rel}")])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("git cat-file failed ({})", out.status));
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// The path `rel` had at commit `sha` when git sees it as renamed since (staged or committed).
pub fn renamed_from(top: &Path, sha: &str, rel: &str) -> Option<String> {
    let out = git(top)
        .args(["diff", "--no-ext-diff", "-M", "--name-status", "-z", "--no-color", sha, "--"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut f = text.split('\0');
    while let Some(status) = f.next().filter(|s| !s.is_empty()) {
        let old = f.next()?;
        if status.starts_with('R') && f.next()? == rel {
            return Some(old.to_string());
        }
    }
    None
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
    #[test]
    fn a_stopped_group_is_empty_even_while_it_forks() {
        for _ in 0..10 {
            // every process ignores SIGTERM and forks all the time
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", "trap '' TERM; while :; do sleep 30 & done"]);
            std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
            let mut child = cmd.spawn().unwrap();
            let pgid = child.id() as i32;
            std::thread::sleep(std::time::Duration::from_millis(100));
            super::end_group(&mut child, pgid, std::time::Duration::ZERO);
            // SAFETY: signal 0 only checks whether the group still has a process
            assert_ne!(unsafe { libc::kill(-pgid, 0) }, 0, "a process of the group lives");
        }
    }

    use super::*;

    #[test]
    fn sidebar_pr_items_are_read() {
        let prs = parse_pr_items(
            br#"[{"number":7,"title":"Fix","author":{"login":"ana"},"isDraft":true,"url":"https://x/7","reviewDecision":"APPROVED","headRefName":"fix","headRefOid":"abc123"},{"title":"no number"},{"number":8}]"#,
        );
        assert_eq!(prs.len(), 2);
        assert_eq!(
            prs[0],
            PrItem {
                number: 7,
                title: "Fix".into(),
                author: "ana".into(),
                draft: true,
                url: "https://x/7".into(),
                review: "APPROVED".into(),
                approvals: 0,
                head: "fix".into(),
                head_oid: "abc123".into(),
            }
        );
        assert_eq!(
            (prs[1].number, prs[1].draft, prs[1].author.as_str()),
            (8, false, "")
        );
        assert_eq!(PrFilter::Review.label(), "Awaiting your review");
    }

    #[test]
    fn without_a_review_decision_the_latest_reviews_decide() {
        let prs = parse_pr_items(
            br#"[{"number":1,"reviewDecision":"","latestReviews":[{"state":"APPROVED"},{"state":"APPROVED"},{"state":"COMMENTED"}]},
                 {"number":2,"reviewDecision":"","latestReviews":[{"state":"APPROVED"},{"state":"CHANGES_REQUESTED"}]},
                 {"number":3,"reviewDecision":"REVIEW_REQUIRED","latestReviews":[{"state":"APPROVED"}]},
                 {"number":4}]"#,
        );
        let got: Vec<(u64, &str, u32)> = prs.iter().map(|p| (p.number, p.review.as_str(), p.approvals)).collect();
        assert_eq!(
            got,
            [
                (1, "APPROVED", 2),
                (2, "CHANGES_REQUESTED", 1),
                (3, "REVIEW_REQUIRED", 1),
                (4, "", 0)
            ]
        );
    }

    #[test]
    fn a_pr_diff_splits_per_file() {
        let d = "diff --git a/A.kt b/A.kt\n--- a/A.kt\n+++ b/A.kt\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/old/B.kt b/new/B.kt\nsimilarity index 90%\n";
        let files = split_diff(d);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, "A.kt");
        assert!(files[0].1.starts_with("diff --git a/A.kt") && files[0].1.ends_with("+b\n"));
        assert_eq!(files[1].0, "new/B.kt");
        assert!(split_diff("").is_empty());
    }

    #[test]
    fn open_pr_list_and_pr_numbers() {
        let prs = parse_pr_list(
            br#"[{"number":12,"title":"Login","headRefName":"login"},{"number":"x"},{"number":9,"headRefName":"b"}]"#,
        );
        assert_eq!(
            prs,
            vec![
                OpenPr {
                    number: 12,
                    title: "Login".into(),
                    head: "login".into()
                },
                OpenPr {
                    number: 9,
                    title: String::new(),
                    head: "b".into()
                },
            ]
        );
        assert!(parse_pr_list(b"not json").is_empty());
        assert_eq!(pr_number("#6110"), Some(6110));
        assert_eq!(pr_number(" 42 "), Some(42));
        assert_eq!(pr_number("feature/x"), None);
    }

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
        let st = status(&dir).unwrap();
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
        assert!(status(&dir).unwrap().iter().all(|c| c.code == '?'));
        assert!(run(&dir, &["checkout", "nope"]).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A fresh folder for a checkout test, a repository `repo` in it with one commit of `files`
    /// (A.kt plus F0.kt…), and a fake gh there that logs its calls to `gh.log`.
    fn checkout_fixture(name: &str, files: usize, gh_script: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("droid-lite-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let dir = dir.canonicalize().unwrap();
        let repo = dir.join("repo");
        let g = |args: &[&str]| run(&repo, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.name", "T"]);
        g(&["config", "user.email", "t@example.com"]);
        std::fs::write(repo.join("A.kt"), "a\n").unwrap();
        for i in 0..files {
            std::fs::write(repo.join(format!("F{i}.kt")), "a\n").unwrap();
        }
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "base"]);
        let gh = dir.join("gh");
        std::fs::write(
            &gh,
            // `pr diff` prints gh.diff (the PR's files), or fails when there is none
            format!("#!/bin/sh\necho \"$*\" >> \"$0.log\"\nif [ \"$1 $2\" = \"pr diff\" ]; then [ -f \"$0.diff\" ] || exit 3; cat \"$0.diff\"; exit 0; fi\n[ \"$1 $2\" = \"pr checkout\" ] || exit 3\n{gh_script}"),
        )
        .unwrap();
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(&gh)
            .status()
            .unwrap();
        (dir, repo, gh)
    }

    /// git with the environment a test needs (an editor that never waits, local submodules).
    fn git_env(repo: &Path, env: &[(&str, &str)], args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["-c", "protocol.file.allow=always"])
            .args(args)
            .envs(env.iter().copied())
            .env("GIT_EDITOR", "true")
            .output()
            .unwrap()
            .status
            .success()
    }

    const TIMEOUT: std::time::Duration = CHECKOUT_TIMEOUT;

    #[test]
    fn checkout_pr_runs_gh_and_never_loses_tracked_work() {
        // a fake gh: PR 7 checks out (only with prompts off), 8 fails after git's progress lines,
        // 9 needs a sign-in, 10 hangs, 11 fails with a very long line
        let (dir, repo, gh) = checkout_fixture(
            "co",
            12,
            "case \"$3\" in\n  7) [ \"$GIT_TERMINAL_PROMPT $GH_PROMPT_DISABLED\" = '0 1' ] || { echo 'fatal: would prompt' >&2; exit 1; }; git checkout -q -b pr-7;;\n  8) echo 'From github.com:x/y' >&2; echo 'failed to run git: fatal: no ref pull/8' >&2; exit 1;;\n  10) ( sleep 3; touch \"$0.late\" ) & wait;;\n  11) printf 'error: %05000d\\n' 0 >&2; exit 1;;\n  *) echo 'please run:  gh auth login' >&2; exit 4;;\nesac\n",
        );
        let g = |args: &[&str]| run(&repo, args).unwrap();
        let gh = gh.as_os_str();
        // untracked files do not block; a gh failure shows its error line, not the progress
        std::fs::write(repo.join("New.kt"), "n\n").unwrap();
        assert_eq!(
            checkout_pr(&repo, 8, gh, TIMEOUT, &Ctl::default()),
            Err("failed to run git: fatal: no ref pull/8".into())
        );
        assert_eq!(
            checkout_pr(&repo, 9, gh, TIMEOUT, &Ctl::default()),
            Err("Not signed in to GitHub: run gh auth login in a terminal".into())
        );
        assert!(
            checkout_pr(
                &repo,
                7,
                "/nonexistent/droid-lite-gh".as_ref(),
                TIMEOUT,
                &Ctl::default()
            )
            .unwrap_err()
            .contains("not installed")
        );
        // a long error line is cut
        let long = checkout_pr(&repo, 11, gh, TIMEOUT, &Ctl::default()).unwrap_err();
        assert_eq!(long.chars().count(), MAX_ERROR_CHARS + 1, "{long}");
        assert!(long.starts_with("error: 0000") && long.ends_with('…'));
        // a gh that hangs is stopped after the timeout, with the git it started
        let t = std::time::Instant::now();
        let err = checkout_pr(
            &repo,
            10,
            gh,
            std::time::Duration::from_secs(1),
            &Ctl::default(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            "Check out timed out; the working tree may be part-switched: run git status"
        );
        assert!(
            t.elapsed() < std::time::Duration::from_millis(2500),
            "{:?}",
            t.elapsed()
        );
        std::thread::sleep(std::time::Duration::from_millis(3000));
        assert!(!dir.join("gh.late").exists(), "the whole group was killed");
        assert_eq!(branch(&repo).as_deref(), Some("main"));
        // changed tracked files: refused, nothing switches, up to 10 files listed
        std::fs::write(repo.join("F0.kt"), "b\n").unwrap();
        g(&["add", "F0.kt"]);
        std::fs::write(repo.join("F0.kt"), "c\n").unwrap(); // staged and unstaged: listed once
        let err = checkout_pr(&repo, 7, gh, TIMEOUT, &Ctl::default()).unwrap_err();
        assert_eq!(err, "Commit or stash your changes first:\nF0.kt");
        for i in 1..12 {
            std::fs::write(repo.join(format!("F{i}.kt")), "b\n").unwrap();
        }
        let err = checkout_pr(&repo, 7, gh, TIMEOUT, &Ctl::default()).unwrap_err();
        let lines: Vec<&str> = err.lines().collect();
        assert_eq!(lines.len(), 12, "{err}");
        assert_eq!(lines[1], "F0.kt");
        assert_eq!(lines[11], "… and 2 more");
        assert_eq!(branch(&repo).as_deref(), Some("main"));
        // a clean tree checks out (prompts off) and reports the new branch
        g(&["reset", "-q", "--hard"]);
        assert_eq!(
            checkout_pr(&repo, 7, gh, TIMEOUT, &Ctl::default()),
            Ok(("pr-7".into(), false))
        );
        assert_eq!(branch(&repo).as_deref(), Some("pr-7"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checkout_pr_refuses_when_git_status_fails() {
        let (dir, repo, gh) = checkout_fixture("costatus", 0, "git checkout -q -b pr\n");
        // a .git file pointing nowhere: git status exits non-zero
        let broken = dir.join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join(".git"), "gitdir: /nonexistent/droid-lite-git\n").unwrap();
        std::fs::write(broken.join("A.kt"), "a\n").unwrap();
        assert!(status(&broken)
            .unwrap_err()
            .starts_with("git status failed: "));
        let err = checkout_pr(&broken, 7, gh.as_os_str(), TIMEOUT, &Ctl::default()).unwrap_err();
        assert!(err.starts_with("git status failed: "), "{err}");
        assert!(!dir.join("gh.log").exists(), "gh never ran");
        assert!(status(&repo).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checkout_pr_refuses_during_a_rebase_or_a_merge() {
        let (dir, repo, gh) = checkout_fixture("coop", 0, "git checkout -q -b pr-7\n");
        let gh = gh.as_os_str();
        let g = |args: &[&str]| assert!(git_env(&repo, &[], args), "{args:?}");
        g(&["checkout", "-q", "-b", "side"]);
        std::fs::write(repo.join("A.kt"), "side\n").unwrap();
        g(&["commit", "-q", "-am", "side"]);
        g(&["checkout", "-q", "main"]);
        std::fs::write(repo.join("A.kt"), "main\n").unwrap();
        g(&["commit", "-q", "-am", "main"]);
        // an interactive rebase stopped at an `edit`: the tree is clean, but it is half done
        let edit = [("GIT_SEQUENCE_EDITOR", "sed -i.bak 's/^pick/edit/'")];
        assert!(git_env(&repo, &edit, &["rebase", "-q", "-i", "HEAD~1"]));
        assert_eq!(operation_in_progress(&repo), Some("rebase"));
        assert!(status(&repo).unwrap().is_empty(), "nothing uncommitted");
        assert_eq!(
            checkout_pr(&repo, 7, gh, TIMEOUT, &Ctl::default()),
            Err("Finish or abort the rebase first".into())
        );
        g(&["rebase", "--abort"]);
        // a merge stopped at a conflict
        assert!(!git_env(&repo, &[], &["merge", "-q", "side"]), "a conflict");
        assert_eq!(operation_in_progress(&repo), Some("merge"));
        assert_eq!(
            checkout_pr(&repo, 7, gh, TIMEOUT, &Ctl::default()),
            Err("Finish or abort the merge first".into())
        );
        g(&["merge", "--abort"]);
        assert!(!dir.join("gh.log").exists(), "gh never ran");
        assert_eq!(operation_in_progress(&repo), None);
        assert_eq!(
            checkout_pr(&repo, 7, gh, TIMEOUT, &Ctl::default()),
            Ok(("pr-7".into(), false))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checkout_pr_refuses_a_detached_head_with_commits_on_no_branch() {
        let (dir, repo, gh) = checkout_fixture("codetached", 0, "git checkout -q -b pr-7\n");
        let gh = gh.as_os_str();
        let g = |args: &[&str]| run(&repo, args).unwrap();
        g(&["checkout", "-q", "--detach"]);
        std::fs::write(repo.join("A.kt"), "lost?\n").unwrap();
        g(&["commit", "-q", "-am", "on no branch"]);
        assert_eq!(
            checkout_pr(&repo, 7, gh, TIMEOUT, &Ctl::default()),
            Err("HEAD has commits on no branch: create a branch first".into())
        );
        assert!(!dir.join("gh.log").exists(), "gh never ran");
        // a tag has that commit: nothing to lose
        g(&["tag", "v1"]);
        assert_eq!(
            checkout_pr(&repo, 7, gh, TIMEOUT, &Ctl::default()),
            Ok(("pr-7".into(), false))
        );
        g(&["checkout", "-q", "v1"]);
        g(&["branch", "-q", "-D", "pr-7"]);
        // detached on a commit a branch has: nothing to lose
        g(&["checkout", "-q", "--detach", "main"]);
        assert_eq!(
            checkout_pr(&repo, 7, gh, TIMEOUT, &Ctl::default()),
            Ok(("pr-7".into(), false))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checkout_pr_ignores_untracked_files_in_submodules_and_asks_for_submodule_update() {
        let (dir, repo, gh) = checkout_fixture("cosub", 0, "git checkout -q pr-sub\n");
        let g = |args: &[&str]| assert!(git_env(&repo, &[], args), "{args:?}");
        // a library repository used as a submodule, with a second commit for the PR
        let lib = dir.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let l = |args: &[&str]| assert!(git_env(&lib, &[], args), "{args:?}");
        l(&["init", "-q", "-b", "main"]);
        l(&["config", "user.name", "T"]);
        l(&["config", "user.email", "t@example.com"]);
        std::fs::write(lib.join("L.kt"), "1\n").unwrap();
        l(&["add", "."]);
        l(&["commit", "-q", "-m", "lib 1"]);
        g(&["submodule", "add", "-q", lib.to_str().unwrap(), "lib"]);
        g(&["commit", "-q", "-m", "add lib"]);
        // the PR's branch moves the submodule to lib's second commit
        g(&["checkout", "-q", "-b", "pr-sub"]);
        let sub = repo.join("lib");
        let s = |args: &[&str]| assert!(git_env(&sub, &[], args), "{args:?}");
        s(&["config", "user.name", "T"]);
        s(&["config", "user.email", "t@example.com"]);
        std::fs::write(sub.join("L.kt"), "2\n").unwrap();
        s(&["commit", "-q", "-am", "lib 2"]);
        g(&["commit", "-q", "-am", "move lib"]);
        g(&["checkout", "-q", "main"]);
        g(&["submodule", "-q", "update"]);
        // an untracked file inside the submodule is not work a checkout can lose
        std::fs::write(sub.join("Scratch.kt"), "s\n").unwrap();
        assert_eq!(
            checkout_pr(&repo, 7, gh.as_os_str(), TIMEOUT, &Ctl::default()),
            Ok(("pr-sub".into(), true)),
            "checked out, and the submodule needs an update"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checkout_pr_stops_gh_with_sigterm_first_then_sigkill() {
        // a fake gh: 12 marks SIGTERM, 13 ignores it with a child left in its group
        let (dir, repo, gh) = checkout_fixture(
            "costop",
            0,
            "case \"$3\" in\n  12) trap 'touch \"$0.term\"; exit 1' TERM; sleep 30 & wait;;\n  13) trap '' TERM; sleep 30 & echo $! > \"$0.pid\"; wait;;\nesac\n",
        );
        let gh = gh.as_os_str();
        // a time-out: SIGTERM, so git can remove its index.lock, and the note says what to check
        let t = std::time::Instant::now();
        let err = checkout_pr(
            &repo,
            12,
            gh,
            std::time::Duration::from_secs(1),
            &Ctl::default(),
        );
        assert_eq!(
            err,
            Err(
                "Check out timed out; the working tree may be part-switched: run git status".into()
            )
        );
        assert!(
            t.elapsed() < std::time::Duration::from_secs(3),
            "{:?}",
            t.elapsed()
        );
        assert!(dir.join("gh.term").exists(), "gh got SIGTERM");
        // stopped from another thread; a gh that ignores SIGTERM is killed after the grace time,
        // with what it started
        let ctl = std::sync::Arc::new(Ctl::default());
        let c = ctl.clone();
        let stopper = std::thread::spawn(move || {
            while c.pgid() == 0 {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            let pgid = c.pgid();
            c.stop();
            pgid
        });
        let t = std::time::Instant::now();
        let err = checkout_pr(&repo, 13, gh, TIMEOUT, &ctl);
        assert_eq!(err, Err("Check out of #13 was stopped".into()));
        assert!(t.elapsed() >= STOP_GRACE, "{:?}", t.elapsed());
        assert!(t.elapsed() < STOP_GRACE + std::time::Duration::from_secs(3));
        assert!(
            stopper.join().unwrap() > 0,
            "the group id was known while gh ran"
        );
        assert_eq!(ctl.pgid(), 0);
        let pid: i32 = std::fs::read_to_string(dir.join("gh.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        // SAFETY: signal 0 only checks whether the process lives
        assert!(
            unsafe { libc::kill(pid, 0) } != 0,
            "gh's child was killed too"
        );
        // stopped before gh started: it never starts
        let log = || std::fs::read_to_string(dir.join("gh.log")).unwrap_or_default();
        let calls = log().lines().count();
        assert_eq!(
            checkout_pr(&repo, 7, gh, TIMEOUT, &ctl),
            Err("Check out of #7 was stopped".into())
        );
        assert_eq!(log().lines().count(), calls, "{}", log());
        assert_eq!(branch(&repo).as_deref(), Some("main"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checkout_pr_lists_files_relative_to_a_project_in_a_subfolder() {
        let (dir, repo, gh) = checkout_fixture("cosubdir", 0, "git checkout -q -b pr-7\n");
        let g = |args: &[&str]| run(&repo, args).unwrap();
        std::fs::create_dir_all(repo.join("android/app")).unwrap();
        std::fs::create_dir_all(repo.join("lib")).unwrap();
        std::fs::write(repo.join("android/app/A.kt"), "a\n").unwrap();
        std::fs::write(repo.join("lib/B.kt"), "b\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "folders"]);
        std::fs::write(repo.join("android/app/A.kt"), "mine\n").unwrap();
        std::fs::write(repo.join("lib/B.kt"), "mine\n").unwrap();
        let project = repo.join("android");
        let err = checkout_pr(&project, 7, gh.as_os_str(), TIMEOUT, &Ctl::default()).unwrap_err();
        assert_eq!(
            err,
            "Commit or stash your changes first:\napp/A.kt\n../lib/B.kt"
        );
        // the Current changes tab gets real paths too, also for what the branch changed
        let changed = status(&project).unwrap();
        assert!(changed.iter().all(|c| c.path.is_file()), "{changed:?}");
        g(&["checkout", "-q", "-b", "side"]);
        g(&["commit", "-q", "-am", "side"]);
        let side = branch_changes(&project, "main");
        assert_eq!(side.len(), 2, "{side:?}");
        assert!(side.iter().all(|c| c.path.is_file()), "{side:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checkout_pr_refuses_to_overwrite_ignored_files_the_pr_changes() {
        let (dir, repo, gh) = checkout_fixture("coignored", 0, "git checkout -q -b pr-7\n");
        let g = |args: &[&str]| run(&repo, args).unwrap();
        std::fs::write(repo.join(".gitignore"), "local.properties\nbuild/\n").unwrap();
        g(&["add", ".gitignore"]);
        g(&["commit", "-q", "-m", "ignore"]);
        std::fs::write(repo.join("local.properties"), "sdk.dir=/mine\n").unwrap();
        std::fs::create_dir_all(repo.join("build")).unwrap();
        std::fs::write(repo.join("build/out.txt"), "o\n").unwrap();
        // the PR changes both ignored files, a tracked one and one that is not here
        std::fs::write(
            dir.join("gh.diff"),
            "A.kt\nlocal.properties\nbuild/out.txt\nNew.kt\n",
        )
        .unwrap();
        let gh = gh.as_os_str();
        assert_eq!(
            checkout_pr(&repo, 7, gh, TIMEOUT, &Ctl::default()),
            Err(
                "Move or back up these ignored files first:\nbuild/out.txt\nlocal.properties"
                    .into()
            )
        );
        let log = std::fs::read_to_string(dir.join("gh.log")).unwrap();
        assert_eq!(log, "pr diff 7 --name-only\n", "gh pr checkout never ran");
        // moved away: it checks out
        std::fs::remove_file(repo.join("local.properties")).unwrap();
        std::fs::remove_file(repo.join("build/out.txt")).unwrap();
        assert_eq!(
            checkout_pr(&repo, 7, gh, TIMEOUT, &Ctl::default()),
            Ok(("pr-7".into(), false))
        );
        // gh cannot list the PR's files: the check is skipped
        g(&["checkout", "-q", "main"]);
        std::fs::write(repo.join("local.properties"), "sdk.dir=/mine\n").unwrap();
        std::fs::remove_file(dir.join("gh.diff")).unwrap();
        g(&["branch", "-q", "-D", "pr-7"]);
        assert_eq!(
            checkout_pr(&repo, 7, gh, TIMEOUT, &Ctl::default()),
            Ok(("pr-7".into(), false))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn operation_in_progress_knows_git_am_and_a_cherry_pick_of_several_commits() {
        let (dir, repo, gh) = checkout_fixture("coam", 0, "git checkout -q -b pr-7\n");
        let g = |args: &[&str]| assert!(git_env(&repo, &[], args), "{args:?}");
        g(&["checkout", "-q", "-b", "side"]);
        std::fs::write(repo.join("A.kt"), "side\n").unwrap();
        g(&["commit", "-q", "-am", "side 1"]);
        std::fs::write(repo.join("B.kt"), "side 2\n").unwrap();
        g(&["add", "B.kt"]);
        g(&["commit", "-q", "-m", "side 2"]);
        g(&[
            "format-patch",
            "-q",
            "-1",
            "HEAD~1",
            "-o",
            dir.to_str().unwrap(),
        ]);
        g(&["checkout", "-q", "main"]);
        std::fs::write(repo.join("A.kt"), "main\n").unwrap();
        g(&["commit", "-q", "-am", "main"]);
        // git am stopped at a conflict: rebase-apply, but no rebase
        let patch = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "patch"))
            .unwrap();
        assert!(!git_env(&repo, &[], &["am", "-q", patch.to_str().unwrap()]));
        assert_eq!(operation_in_progress(&repo), Some("git am"));
        assert_eq!(
            checkout_pr(&repo, 7, gh.as_os_str(), TIMEOUT, &Ctl::default()),
            Err("Finish or abort the git am first".into())
        );
        g(&["am", "--abort"]);
        // two commits to cherry-pick, the first resolved and committed: the second still waits
        assert!(!git_env(&repo, &[], &["cherry-pick", "side~1", "side"]));
        std::fs::write(repo.join("A.kt"), "both\n").unwrap();
        g(&["commit", "-q", "-am", "resolved"]);
        assert_eq!(operation_in_progress(&repo), Some("cherry-pick or revert"));
        g(&["cherry-pick", "--abort"]);
        assert_eq!(operation_in_progress(&repo), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_git_diff_is_an_error_and_a_repo_diff_tool_never_runs() {
        assert!(diff_no_index("a\n", "b\n", &["--no-such-flag"]).is_err());
        assert_eq!(line_diff("a\n", "a\n"), Ok(Vec::new()));
        // a diff tool set by the environment (as diff.external in a repo) would print this
        let ran = std::env::temp_dir().join(format!("dl-ext-diff-{}", std::process::id()));
        let tool = ran.with_extension("sh");
        std::fs::write(&tool, format!("#!/bin/sh\ntouch {}\n", ran.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
        let dir = private_temp_dir("dl-ext").unwrap();
        let (a, b) = (dir.join("old"), dir.join("new"));
        new_private_file(&a, "a\n").unwrap();
        new_private_file(&b, "b\n").unwrap();
        assert!(new_private_file(&a, "x").is_err(), "never over a file that is there");
        let out = no_index_cmd(&a, &b, &[])
            .env("GIT_EXTERNAL_DIFF", &tool)
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("+b"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&tool);
        assert!(!ran.exists(), "the external diff tool ran");
    }

    #[test]
    fn diff_temp_files_live_in_a_new_private_folder() {
        use std::os::unix::fs::PermissionsExt;
        let a = private_temp_dir("dl-priv").unwrap();
        let b = private_temp_dir("dl-priv").unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o700);
        new_private_file(&a.join("old"), "x").unwrap();
        assert_eq!(std::fs::metadata(a.join("old")).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
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
